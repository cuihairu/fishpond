//! F025 acceptance (operations): the idempotent create over the F014
//! reliable-task store — mandatory `Idempotency-Key`, replay returns
//! the original (200), key reuse with different work is a 409 — plus
//! the claim CAS (PENDING → RUNNING, loser 409), attempt close
//! semantics (succeeded/failed terminal, timed-out requeues with the
//! next attempt number), scope enforcement and tenant isolation.
//! Skips without a database.

#[allow(dead_code)]
mod common;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::{gated, tenant_record, unique_entity, unique_tenant};
use fishpond_domain::TenantId;
use fishpond_server::http::auth::{RestKeyRecord, RestKeys};
use fishpond_server::{AppState, build_router};
use fishpond_storage::Store;
use http_body_util::BodyExt;
use tower::ServiceExt;

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
            scopes: vec!["operations:read".to_owned(), "operations:write".to_owned()],
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
    idempotency_key: Option<&str>,
    body: Option<serde_json::Value>,
) -> (StatusCode, serde_json::Value) {
    let mut builder = Request::builder().method(method).uri(uri);
    if let Some(key) = key {
        builder = builder.header("authorization", format!("Bearer {key}"));
    }
    if let Some(idem) = idempotency_key {
        builder = builder.header("idempotency-key", idem);
    }
    if body.is_some() {
        // Axum's Json extractor refuses a body without the header.
        builder = builder.header("content-type", "application/json");
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
async fn operations_idempotent_create_claim_and_close() {
    if gated().await.is_none() {
        eprintln!("skip: no database");
        return;
    }
    let (router, _tenant, _store) = setup("f025-op").await;
    let key = "key-f025-op";
    let id = unique_entity("op", "idem");
    let idem = unique_entity("idem", "key");

    // No Idempotency-Key header → 400 problem.
    let (status, problem) = call(
        &router,
        "POST",
        "/operations",
        Some(key),
        None,
        Some(serde_json::json!({ "id": id, "kind": "send_message" })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{problem}");
    assert_eq!(
        problem["type"],
        "https://fishpond.dev/problems/missing-idempotency-key"
    );

    // Create (201, PENDING), replay (200, same operation), key reuse
    // with different work (409).
    let (status, created) = call(
        &router,
        "POST",
        "/operations",
        Some(key),
        Some(idem.as_str()),
        Some(serde_json::json!({ "id": id, "kind": "send_message" })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    assert_eq!(created["replayed"], false);
    assert_eq!(created["operation"]["state"], "PENDING");
    assert_eq!(created["operation"]["kind"], "send_message");

    let (status, replay) = call(
        &router,
        "POST",
        "/operations",
        Some(key),
        Some(idem.as_str()),
        Some(serde_json::json!({ "id": id, "kind": "send_message" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{replay}");
    assert_eq!(replay["replayed"], true);
    assert_eq!(replay["operation"]["id"], id.as_str());

    let (status, conflict) = call(
        &router,
        "POST",
        "/operations",
        Some(key),
        Some(idem.as_str()),
        Some(serde_json::json!({ "id": unique_entity("op", "other"), "kind": "send_message" })),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{conflict}");
    assert_eq!(conflict["type"], "https://fishpond.dev/problems/conflict");

    // Read back; a missing one is a 404.
    let path = format!("/operations/{id}");
    let (status, got) = call(&router, "GET", &path, Some(key), None, None).await;
    assert_eq!(status, StatusCode::OK, "{got}");
    assert_eq!(got["state"], "PENDING");
    let (status, problem) = call(
        &router,
        "GET",
        "/operations/op-never-created",
        Some(key),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{problem}");

    // Claim: PENDING → RUNNING with attempt 1; a competing claim is a 409.
    let worker = unique_entity("wkr", "one");
    let (status, claimed) = call(
        &router,
        "POST",
        &format!("{path}/claim"),
        Some(key),
        None,
        Some(serde_json::json!({
            "worker_id": worker,
            "attempt_id": unique_entity("att", "one"),
            "deadline_at": 9_000_000i64,
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{claimed}");
    assert_eq!(claimed["operation"]["state"], "RUNNING");
    assert_eq!(claimed["attempt"]["attempt_no"], 1);
    let (status, problem) = call(
        &router,
        "POST",
        &format!("{path}/claim"),
        Some(key),
        None,
        Some(serde_json::json!({
            "worker_id": unique_entity("wkr", "two"),
            "attempt_id": unique_entity("att", "two"),
            "deadline_at": 9_000_000i64,
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{problem}");

    // Close by the wrong worker: the attempt is not theirs (404).
    let attempt_one = claimed["attempt"]["attempt_id"].as_str().expect("attempt");
    let (status, problem) = call(
        &router,
        "POST",
        &format!("/operations/{id}/attempts/{attempt_one}/close"),
        Some(key),
        None,
        Some(
            serde_json::json!({ "worker_id": unique_entity("wkr", "evil"), "result": "succeeded" }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{problem}");

    // Timed-out requeues: RUNNING → PENDING, next claim is attempt 2.
    let (status, closed) = call(
        &router,
        "POST",
        &format!("/operations/{id}/attempts/{attempt_one}/close"),
        Some(key),
        None,
        Some(serde_json::json!({ "worker_id": worker, "result": "timed-out" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{closed}");
    assert_eq!(closed["state"], "PENDING");
    let attempt_two = unique_entity("att", "three");
    let (status, reclaimed) = call(
        &router,
        "POST",
        &format!("{path}/claim"),
        Some(key),
        None,
        Some(serde_json::json!({
            "worker_id": worker,
            "attempt_id": attempt_two,
            "deadline_at": 9_000_000i64,
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{reclaimed}");
    assert_eq!(reclaimed["attempt"]["attempt_no"], 2);

    // Double close is a 409; success is terminal.
    let (status, problem) = call(
        &router,
        "POST",
        &format!("/operations/{id}/attempts/{attempt_one}/close"),
        Some(key),
        None,
        Some(serde_json::json!({ "worker_id": worker, "result": "succeeded" })),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{problem}");
    let (status, done) = call(
        &router,
        "POST",
        &format!("/operations/{id}/attempts/{attempt_two}/close"),
        Some(key),
        None,
        Some(serde_json::json!({ "worker_id": worker, "result": "succeeded" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{done}");
    assert_eq!(done["state"], "SUCCEEDED");

    // Terminal operations cannot be claimed again (409).
    let (status, problem) = call(
        &router,
        "POST",
        &format!("{path}/claim"),
        Some(key),
        None,
        Some(serde_json::json!({
            "worker_id": worker,
            "attempt_id": unique_entity("att", "late"),
            "deadline_at": 9_000_000i64,
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{problem}");

    // Unknown close result (422).
    let other = unique_entity("op", "bad");
    call(
        &router,
        "POST",
        "/operations",
        Some(key),
        Some(unique_entity("idem", "bad").as_str()),
        Some(serde_json::json!({ "id": other, "kind": "send_message" })),
    )
    .await;
    let (status, problem) = call(
        &router,
        "POST",
        &format!("/operations/{other}/attempts/att-x/close"),
        Some(key),
        None,
        Some(serde_json::json!({ "worker_id": worker, "result": "teleport" })),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{problem}");

    // Scope + auth: the accounts-only key cannot even read (403), no
    // key is 401.
    let (status, problem) = call(
        &router,
        "GET",
        "/operations",
        Some("key-ro-f025-op"),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{problem}");
    assert!(
        problem["detail"]
            .as_str()
            .expect("detail")
            .contains("operations:read")
    );
    let (status, problem) = call(&router, "GET", "/operations", None, None, None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{problem}");
}

#[tokio::test]
async fn operations_list_paginates_and_stays_tenant_scoped() {
    if gated().await.is_none() {
        eprintln!("skip: no database");
        return;
    }
    let (router, tenant, store) = setup("f025-oplist").await;
    let key = "key-f025-oplist";

    let mut ids = Vec::new();
    for n in 0..3 {
        let id = unique_entity("op", &format!("list{n}"));
        let (status, created) = call(
            &router,
            "POST",
            "/operations",
            Some(key),
            Some(unique_entity("idem", &format!("list{n}")).as_str()),
            Some(serde_json::json!({ "id": id, "kind": "send_message" })),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{created}");
        ids.push(id);
    }
    ids.sort();

    // Cursor walk with limit 2: two pages of 2 and 1, no third cursor.
    let (status, page1) = call(&router, "GET", "/operations?limit=2", Some(key), None, None).await;
    assert_eq!(status, StatusCode::OK, "{page1}");
    let first = page1["operations"].as_array().expect("page one");
    assert_eq!(first.len(), 2);
    let cursor1 = page1["next_cursor"].as_str().expect("cursor").to_owned();
    let (status, page2) = call(
        &router,
        "GET",
        &format!("/operations?limit=2&cursor={cursor1}"),
        Some(key),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{page2}");
    let second = page2["operations"].as_array().expect("page two");
    assert_eq!(second.len(), 1);
    assert_eq!(page2["next_cursor"], serde_json::Value::Null);
    let mut seen: Vec<String> = first
        .iter()
        .chain(second.iter())
        .map(|operation| operation["id"].as_str().expect("id").to_owned())
        .collect();
    seen.sort();
    assert_eq!(seen, ids, "the walk covers exactly the created operations");

    // Tenant isolation: another tenant's key cannot see the operation.
    let other = unique_tenant("f025-oplist-other");
    store
        .upsert_tenant(&tenant_record(&other, "f025-oplist-other"))
        .await
        .expect("other tenant");
    let mut keys = RestKeys::new();
    keys.insert(
        "key-oplist-other",
        RestKeyRecord {
            tenant_id: other,
            scopes: vec!["operations:read".to_owned()],
        },
    );
    let isolated = build_router(AppState::new(store.clone(), keys));
    let (status, page) = call(
        &isolated,
        "GET",
        "/operations",
        Some("key-oplist-other"),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{page}");
    assert!(page["operations"].as_array().expect("list").is_empty());
    let _ = tenant;
}
