//! F034 HTTP-face acceptance: the takeover routes against real rows —
//! issue (token shown exactly once), possession-only verify, release,
//! the audit list, scope enforcement, and cross-tenant isolation.
//! Skips without a database, like every other DB-backed suite.

#[allow(dead_code)]
mod common;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::{gated, tenant_record, ts, unique_entity, unique_tenant};
use fishpond_domain::{AccountId, AuthSessionId, TenantId};
use fishpond_server::http::auth::{RestKeyRecord, RestKeys};
use fishpond_server::{AppState, SessionCoordinator, Sessions, build_router};
use fishpond_storage::{CredentialCipher, MasterKey, NewCredential, SecretBox};
use http_body_util::BodyExt;
use tower::ServiceExt;

/// Everything one test needs: a world with an authorized session plus
/// a router whose admin key carries the takeover scope.
struct World {
    router: Router,
    tenant: TenantId,
    session: String,
    key: String,
    /// A key of the same tenant without the takeover scope.
    ro_key: String,
    /// A key of a *different* tenant.
    foreign_key: String,
}

async fn world(tag: &str) -> Option<World> {
    let store = gated().await?;
    let tenant = unique_tenant(tag);
    store
        .upsert_tenant(&tenant_record(&tenant, tag))
        .await
        .expect("tenant upsert");
    let foreign = unique_tenant(&format!("{tag}-foreign"));
    store
        .upsert_tenant(&tenant_record(&foreign, &format!("{tag}-foreign")))
        .await
        .expect("foreign tenant upsert");

    let account_id = AccountId::parse(unique_entity("acc", tag)).expect("account id");
    store
        .upsert_account(&common::account(
            account_id.as_str(),
            &tenant,
            fishpond_domain::AccountState::Online,
        ))
        .await
        .expect("account upsert");
    let credential = unique_entity("cred", tag);
    let cipher = CredentialCipher::new(MasterKey::generate(), 1);
    store
        .store_credential(NewCredential {
            id: &credential,
            tenant: &tenant,
            platform: "mock",
            kind: "api_token",
            cipher: &cipher,
            secret: SecretBox::new("f034-http-secret"),
            created_at: ts(900),
        })
        .await
        .expect("credential store");

    let sessions = Sessions::new(store.clone());
    let coordinator = SessionCoordinator::new(sessions.clone());
    let session = AuthSessionId::parse(unique_entity("sess", tag)).expect("session id");
    sessions
        .start(
            &tenant,
            session.clone(),
            account_id.clone(),
            ts(1_000),
            None,
        )
        .await
        .expect("session start");
    let version = sessions
        .credential_version(&tenant, &credential)
        .await
        .expect("credential version");
    coordinator
        .authorize_exclusive(&tenant, &session, &credential, version, None, ts(2_000))
        .await
        .expect("exclusive authorize");

    let key = format!("key-{tag}");
    let ro_key = format!("key-ro-{tag}");
    let foreign_key = format!("key-foreign-{tag}");
    let mut keys = RestKeys::new();
    keys.insert(
        key.clone(),
        RestKeyRecord {
            tenant_id: tenant.clone(),
            scopes: vec![
                "sessions:read".to_owned(),
                "sessions:write".to_owned(),
                "sessions:takeover".to_owned(),
            ],
        },
    );
    keys.insert(
        ro_key.clone(),
        RestKeyRecord {
            tenant_id: tenant.clone(),
            scopes: vec!["sessions:read".to_owned()],
        },
    );
    keys.insert(
        foreign_key.clone(),
        RestKeyRecord {
            tenant_id: foreign,
            scopes: vec![
                "sessions:read".to_owned(),
                "sessions:write".to_owned(),
                "sessions:takeover".to_owned(),
            ],
        },
    );
    let state = AppState::new(store.clone(), keys);
    Some(World {
        router: build_router(state),
        tenant,
        session: session.as_str().to_owned(),
        key,
        ro_key,
        foreign_key,
    })
}

/// One HTTP call; `extra_headers` carries the takeover token.
async fn call(
    router: &Router,
    method: &str,
    uri: &str,
    key: Option<&str>,
    body: Option<serde_json::Value>,
    token_header: Option<&str>,
) -> (StatusCode, serde_json::Value) {
    let mut builder = Request::builder().method(method).uri(uri);
    if let Some(key) = key {
        builder = builder.header("authorization", format!("Bearer {key}"));
    }
    if body.is_some() {
        builder = builder.header("content-type", "application/json");
    }
    if let Some(token) = token_header {
        builder = builder.header("x-takeover-token", token);
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
async fn takeover_routes_issue_verify_and_release() {
    let Some(world) = world("f034-http").await else {
        eprintln!("skip: no database");
        return;
    };
    let uri = format!("/sessions/{}/takeover", world.session);

    // Issue: 201, the token appears exactly once, prefixed.
    let (status, issued) = call(
        &world.router,
        "POST",
        &uri,
        Some(&world.key),
        Some(serde_json::json!({ "operator": "op-http", "ttl_ms": 60_000 })),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{issued}");
    let token = issued["token"].as_str().expect("token").to_owned();
    let grant_id = issued["grant_id"].as_str().expect("grant id").to_owned();
    assert!(token.starts_with("fp_tk_"), "{token}");
    assert!(!grant_id.is_empty());
    assert!(issued["expires_at"].as_i64().expect("expires") > 0);

    // Verify: possession alone — no bearer key in the call at all.
    let (status, context) = call(
        &world.router,
        "POST",
        "/takeover/verify",
        None,
        None,
        Some(&token),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{context}");
    assert_eq!(context["grant_id"], grant_id.as_str());
    assert_eq!(context["tenant"], world.tenant.as_str());
    assert_eq!(context["operator"], "op-http");
    assert_eq!(context["session"], world.session.as_str());

    // The audit list shows the grant with a hint, never the token.
    let (status, listed) = call(&world.router, "GET", &uri, Some(&world.key), None, None).await;
    assert_eq!(status, StatusCode::OK, "{listed}");
    let grants = listed["grants"].as_array().expect("grants");
    assert_eq!(grants.len(), 1);
    assert_eq!(grants[0]["id"], grant_id.as_str());
    assert!(
        !listed.to_string().contains(&token),
        "audit must not carry the raw token"
    );

    // Release: the credential dies and the session resumes.
    let release_uri = format!("/sessions/{}/takeover/{grant_id}", world.session);
    let (status, released) = call(
        &world.router,
        "DELETE",
        &release_uri,
        Some(&world.key),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{released}");
    assert_eq!(released["released"], true);
    let (status, dead) = call(
        &world.router,
        "POST",
        "/takeover/verify",
        None,
        None,
        Some(&token),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{dead}");
}

#[tokio::test]
async fn scopes_isolation_and_problem_classes() {
    let Some(world) = world("f034-http2").await else {
        eprintln!("skip: no database");
        return;
    };
    let uri = format!("/sessions/{}/takeover", world.session);

    // The takeover scope is enforced: a read-only tenant key is 403.
    let (status, _) = call(
        &world.router,
        "POST",
        &uri,
        Some(&world.ro_key),
        Some(serde_json::json!({ "operator": "op-x", "ttl_ms": 60_000 })),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // An unknown session is a 404 problem, a zero ttl a 422.
    let ghost = format!("/sessions/{}/takeover", unique_entity("sess", "ghost"));
    let (status, body) = call(
        &world.router,
        "POST",
        &ghost,
        Some(&world.key),
        Some(serde_json::json!({ "operator": "op-x", "ttl_ms": 60_000 })),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    let (status, body) = call(
        &world.router,
        "POST",
        &uri,
        Some(&world.key),
        Some(serde_json::json!({ "operator": "op-x", "ttl_ms": 0 })),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");

    // Verify without presenting any token is 401.
    let (status, _) = call(&world.router, "POST", "/takeover/verify", None, None, None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // A live grant blocks re-issue with a conflict problem.
    let (_, issued) = call(
        &world.router,
        "POST",
        &uri,
        Some(&world.key),
        Some(serde_json::json!({ "operator": "op-x", "ttl_ms": 60_000 })),
        None,
    )
    .await;
    assert!(issued["token"].as_str().expect("token").len() > 4);
    let token = issued["token"].as_str().expect("token").to_owned();
    let grant_id = issued["grant_id"].as_str().expect("grant id").to_owned();
    let (status, body) = call(
        &world.router,
        "POST",
        &uri,
        Some(&world.key),
        Some(serde_json::json!({ "operator": "op-y", "ttl_ms": 60_000 })),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");

    // The foreign tenant cannot release (404 no-op) and sees nothing
    // through the audit list — the takeover survives untouched.
    let release_uri = format!("/sessions/{}/takeover/{grant_id}", world.session);
    let (status, _) = call(
        &world.router,
        "DELETE",
        &release_uri,
        Some(&world.foreign_key),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, listed) = call(
        &world.router,
        "GET",
        &uri,
        Some(&world.foreign_key),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{listed}");
    assert!(listed["grants"].as_array().expect("grants").is_empty());
    let (status, alive) = call(
        &world.router,
        "POST",
        "/takeover/verify",
        None,
        None,
        Some(&token),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{alive}");
}
