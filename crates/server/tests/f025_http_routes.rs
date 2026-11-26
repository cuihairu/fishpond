//! F025 acceptance (routes): the account and session REST contracts
//! against the real services and database — tenant scoping from the
//! bearer key, scope enforcement (403), Problem documents for every
//! failure class (404/409/422), cursor pagination on the session list,
//! and the session transition surface. Skips without a database, like
//! every other DB-backed suite.

#[allow(dead_code)]
mod common;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::{gated, tenant_record, ts, unique_entity, unique_tenant};
use fishpond_domain::TenantId;
use fishpond_server::http::auth::{RestKeyRecord, RestKeys};
use fishpond_server::{AppState, build_router};
use fishpond_storage::Store;
use http_body_util::BodyExt;
use tower::ServiceExt;

/// A router with one key bound to one fresh tenant.
async fn setup(tag: &str) -> (Router, TenantId, Store) {
    let store = gated().await.expect("checked by caller");
    let tenant = unique_tenant(tag);
    store
        .upsert_tenant(&tenant_record(&tenant, tag))
        .await
        .expect("tenant insert");
    let mut keys = RestKeys::new();
    keys.insert(
        format!("key-{tag}"),
        RestKeyRecord {
            tenant_id: tenant.clone(),
            scopes: vec![
                "accounts:read".to_owned(),
                "accounts:write".to_owned(),
                "sessions:read".to_owned(),
                "sessions:write".to_owned(),
            ],
        },
    );
    keys.insert(
        format!("key-ro-{tag}"),
        RestKeyRecord {
            tenant_id: tenant.clone(),
            scopes: vec!["accounts:read".to_owned()],
        },
    );
    let state = AppState::new(store.clone(), keys);
    (build_router(state), tenant, store)
}

async fn call(
    router: &Router,
    method: &str,
    uri: &str,
    key: Option<&str>,
    body: Option<serde_json::Value>,
) -> (StatusCode, serde_json::Value) {
    let mut builder = Request::builder().method(method).uri(uri);
    if let Some(key) = key {
        builder = builder.header("authorization", format!("Bearer {key}"));
    }
    let request = builder
        .body(Body::from(
            body.map(|value| value.to_string())
                .unwrap_or_else(|| "{}".to_owned()),
        ))
        .expect("request");
    let response = router.clone().oneshot(request).await.expect("response");
    let status = response.status();
    let bytes = BodyExt::collect(response.into_body())
        .await
        .expect("body")
        .to_bytes();
    let value: serde_json::Value =
        serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, value)
}

#[tokio::test]
async fn accounts_crud_scopes_and_problems() {
    if gated().await.is_none() {
        eprintln!("skip: no database");
        return;
    }
    let (router, tenant, _store) = setup("f025-acc").await;
    let account_id = unique_entity("acc", "crud");
    let path = format!("/accounts/{account_id}");

    // Register (201), duplicate (409), get (200), foreign/missing (404).
    let (status, created) = call(
        &router,
        "POST",
        "/accounts",
        Some("key-f025-acc"),
        Some(serde_json::json!({ "id": account_id, "platform": "mock" })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    assert_eq!(created["id"], account_id.as_str());
    assert_eq!(created["state"], "OFFLINE");
    assert_eq!(created["platform"], "mock");

    let (status, problem) = call(
        &router,
        "POST",
        "/accounts",
        Some("key-f025-acc"),
        Some(serde_json::json!({ "id": account_id, "platform": "mock" })),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{problem}");
    assert_eq!(problem["type"], "https://fishpond.dev/problems/conflict");

    let (status, got) = call(&router, "GET", &path, Some("key-f025-acc"), None).await;
    assert_eq!(status, StatusCode::OK, "{got}");
    assert_eq!(got["id"], account_id.as_str());

    let (status, problem) = call(
        &router,
        "GET",
        "/accounts/acc-never-registered",
        Some("key-f025-acc"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{problem}");
    assert_eq!(problem["type"], "https://fishpond.dev/problems/not-found");

    // State machine step: OFFLINE → ONLINE, then an illegal step (409).
    let (status, moved) = call(
        &router,
        "POST",
        &format!("{path}/state"),
        Some("key-f025-acc"),
        Some(serde_json::json!({ "state": "ONLINE" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{moved}");
    assert_eq!(moved["state"], "ONLINE");

    // An illegal step (ONLINE has no self-transition) is a 409.
    let (status, problem) = call(
        &router,
        "POST",
        &format!("{path}/state"),
        Some("key-f025-acc"),
        Some(serde_json::json!({ "state": "ONLINE" })),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{problem}");
    assert_eq!(problem["type"], "https://fishpond.dev/problems/conflict");

    // Malformed body: unknown state name (422 invalid).
    let (status, problem) = call(
        &router,
        "POST",
        &format!("{path}/state"),
        Some("key-f025-acc"),
        Some(serde_json::json!({ "state": "SLEEPING" })),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{problem}");
    assert_eq!(problem["type"], "https://fishpond.dev/problems/invalid");

    // List contains the account; status counts it as online.
    let (status, listed) = call(&router, "GET", "/accounts", Some("key-f025-acc"), None).await;
    assert_eq!(status, StatusCode::OK, "{listed}");
    assert_eq!(listed["accounts"].as_array().expect("list").len(), 1);
    let (status, summary) = call(&router, "GET", "/status", Some("key-f025-acc"), None).await;
    assert_eq!(status, StatusCode::OK, "{summary}");
    assert_eq!(summary["online"], 1);
    assert_eq!(summary["total"], 1);

    // Scope enforcement: the read-only key cannot write (403), and no
    // key at all is 401.
    let (status, problem) = call(
        &router,
        "POST",
        "/accounts",
        Some("key-ro-f025-acc"),
        Some(serde_json::json!({ "id": "acc-other", "platform": "mock" })),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{problem}");
    assert_eq!(
        problem["type"],
        "https://fishpond.dev/problems/scope-denied"
    );
    let (status, problem) = call(&router, "GET", "/accounts", None, None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{problem}");

    // Tenant isolation: a second tenant's key cannot see the account.
    let other = unique_tenant("f025-acc-other");
    _store
        .upsert_tenant(&tenant_record(&other, "f025-acc-other"))
        .await
        .expect("other tenant");
    let mut keys = RestKeys::new();
    keys.insert(
        "key-other-tenant",
        RestKeyRecord {
            tenant_id: other,
            scopes: vec!["accounts:read".to_owned()],
        },
    );
    let isolated = build_router(AppState::new(_store.clone(), keys));
    let (status, listed) = call(
        &isolated,
        "GET",
        "/accounts",
        Some("key-other-tenant"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{listed}");
    assert!(
        listed["accounts"].as_array().expect("list").is_empty(),
        "another tenant's key must see nothing: {listed}"
    );
    let _ = tenant;
}

#[tokio::test]
async fn sessions_start_list_cursor_and_transitions() {
    if gated().await.is_none() {
        eprintln!("skip: no database");
        return;
    }
    let (router, tenant, store) = setup("f025-ses").await;
    let key = "key-f025-ses";

    // An account to log into.
    let account_id = unique_entity("acc", "ses");
    let account = fishpond_domain::Account::new(
        fishpond_domain::AccountId::parse(account_id.clone()).expect("id"),
        tenant.clone(),
        "mock",
        fishpond_domain::AccountState::Online,
        None,
        ts(2_000),
    )
    .expect("account");
    store
        .upsert_account(&account)
        .await
        .expect("account insert");

    // Start three sessions (client-chosen ids, unique per tenant).
    let mut ids = Vec::new();
    for n in 0..3 {
        let id = unique_entity("ses", &format!("list{n}"));
        let (status, created) = call(
            &router,
            "POST",
            "/sessions",
            Some(key),
            Some(serde_json::json!({
                "id": id,
                "account_id": account_id,
                "expires_at": 9_000_000i64,
            })),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{created}");
        assert_eq!(created["state"], "PENDING");
        ids.push(id);
    }
    ids.sort();

    // Cursor walk: limit 2 → page 1 + cursor, page 2 + cursor, page 3 empty.
    let (status, page1) = call(&router, "GET", "/sessions?limit=2", Some(key), None).await;
    assert_eq!(status, StatusCode::OK, "{page1}");
    let first = page1["sessions"].as_array().expect("page one");
    assert_eq!(first.len(), 2);
    assert_eq!(first[0]["state"], "PENDING");
    let cursor1 = page1["next_cursor"].as_str().expect("cursor").to_owned();

    let (status, page2) = call(
        &router,
        "GET",
        &format!("/sessions?limit=2&cursor={cursor1}"),
        Some(key),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{page2}");
    let second = page2["sessions"].as_array().expect("page two");
    assert_eq!(second.len(), 1);
    assert_eq!(page2["next_cursor"], serde_json::Value::Null);
    let mut seen: Vec<String> = first
        .iter()
        .chain(second.iter())
        .map(|session| session["id"].as_str().expect("id").to_owned())
        .collect();
    seen.sort();
    assert_eq!(seen, ids, "the walk covers exactly the started sessions");

    // A mangled cursor is a 400 problem, not a 500.
    let (status, problem) =
        call(&router, "GET", "/sessions?cursor=nonsense", Some(key), None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{problem}");
    assert_eq!(
        problem["type"],
        "https://fishpond.dev/problems/invalid-cursor"
    );

    // Transitions: confirm, then expire. (authorize is exercised by
    // F026's end-to-end flow — it needs a real credential row.)
    let session_id = &ids[0];
    let (status, confirmed) = call(
        &router,
        "POST",
        &format!("/sessions/{session_id}/transition"),
        Some(key),
        Some(serde_json::json!({ "action": "confirm", "note": "qr scanned" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{confirmed}");
    assert_eq!(confirmed["state"], "AWAITING_CONFIRMATION");
    assert_eq!(confirmed["state_note"], "qr scanned");

    let (status, expired) = call(
        &router,
        "POST",
        &format!("/sessions/{session_id}/transition"),
        Some(key),
        Some(serde_json::json!({ "action": "expire" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{expired}");
    assert_eq!(expired["state"], "EXPIRED");

    // An illegal transition (confirm a terminal session) is a 409.
    let (status, problem) = call(
        &router,
        "POST",
        &format!("/sessions/{session_id}/transition"),
        Some(key),
        Some(serde_json::json!({ "action": "confirm" })),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{problem}");

    // Unknown action (422), authorize without a credential (422),
    // authorize with a missing credential (404), bad id (422).
    let (status, problem) = call(
        &router,
        "POST",
        &format!("/sessions/{session_id}/transition"),
        Some(key),
        Some(serde_json::json!({ "action": "teleport" })),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{problem}");

    let live_id = &ids[1];
    let (status, problem) = call(
        &router,
        "POST",
        &format!("/sessions/{live_id}/transition"),
        Some(key),
        Some(serde_json::json!({ "action": "authorize" })),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{problem}");
    let (status, problem) = call(
        &router,
        "POST",
        &format!("/sessions/{live_id}/transition"),
        Some(key),
        Some(serde_json::json!({
            "action": "authorize",
            "credential_id": "cred-missing",
            "credential_version": 1,
        })),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{problem}");
    assert_eq!(problem["detail"], "credential cred-missing not found");

    // GET one session; foreign key scope: sessions need sessions:read —
    // the read-only accounts key is denied (403).
    let (status, _got) = call(
        &router,
        "GET",
        &format!("/sessions/{live_id}"),
        Some(key),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, problem) = call(
        &router,
        "GET",
        &format!("/sessions/{live_id}"),
        Some("key-ro-f025-ses"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{problem}");
}
