//! F025 acceptance (increment A): the REST plumbing — health, the
//! served OpenAPI document, the RFC 9457 problem mapping, and the
//! bearer-key auth extractor. Pure tests: no database, no sockets
//! (the extractor is called directly; route-level contract tests land
//! with the routes).

use axum::body::Body;
use axum::extract::FromRequestParts;
use axum::http::{Request, StatusCode};
use fishpond_domain::TenantId;
use fishpond_server::error::ServiceError;
use fishpond_server::http::auth::{Caller, RestKeyRecord, RestKeys};
use fishpond_server::http::problem::Problem;
use fishpond_server::{AppState, build_router};
use tower::ServiceExt;

fn test_state() -> AppState {
    let mut keys = RestKeys::new();
    keys.insert(
        "key-good",
        RestKeyRecord {
            tenant_id: TenantId::parse("t-tenant-a").expect("tenant id"),
            scopes: vec!["accounts:read".to_owned(), "events:read".to_owned()],
        },
    );
    AppState { keys }
}

async fn body_string(body: Body) -> String {
    let bytes = http_body_util::BodyExt::collect(body)
        .await
        .expect("body")
        .to_bytes();
    String::from_utf8(bytes.to_vec()).expect("utf-8")
}

async fn parts_with_auth(header: Option<&str>) -> axum::http::request::Parts {
    let mut builder = Request::builder().uri("/accounts");
    if let Some(value) = header {
        builder = builder.header("authorization", value);
    }
    let (parts, _) = builder.body(()).expect("request").into_parts();
    parts
}

#[tokio::test]
async fn health_reports_service_and_version() {
    let response = build_router(test_state())
        .oneshot(
            Request::builder()
                .uri("/healthz")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::OK);
    let content_type = response
        .headers()
        .get("content-type")
        .expect("content type")
        .to_str()
        .expect("ascii")
        .to_owned();
    assert!(
        content_type.starts_with("application/json"),
        "{content_type}"
    );
    let text = body_string(response.into_body()).await;
    let health: serde_json::Value = serde_json::from_str(&text).expect("json");
    assert_eq!(health["status"], "ok");
    assert_eq!(health["service"], "fishpond-rest");
    assert_eq!(health["version"], env!("CARGO_PKG_VERSION"));
}

#[tokio::test]
async fn openapi_document_is_served_and_contract_frozen() {
    let response = build_router(test_state())
        .oneshot(
            Request::builder()
                .uri("/openapi.json")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::OK);
    let text = body_string(response.into_body()).await;
    let doc: serde_json::Value = serde_json::from_str(&text).expect("json");

    // Structural contract: the version, the documented paths and the
    // problem schema stay stable — clients generate SDKs from this
    // (F036).
    assert_eq!(doc["openapi"], "3.1.0");
    for path in ["/healthz", "/accounts", "/events", "/events/stream"] {
        assert!(
            doc["paths"].get(path).is_some(),
            "path {path} missing from the document"
        );
    }
    assert_eq!(
        doc["components"]["schemas"]["Problem"]["properties"]["status"]["type"],
        "integer"
    );
    assert_eq!(
        doc["components"]["securitySchemes"]["bearerAuth"]["scheme"],
        "bearer"
    );
}

#[test]
fn service_errors_map_to_problem_classes() {
    let not_found = Problem::from(&ServiceError::not_found("account", "acc-1"));
    assert_eq!(not_found.status(), StatusCode::NOT_FOUND);
    assert_eq!(
        not_found.type_uri,
        "https://fishpond.dev/problems/not-found"
    );
    assert_eq!(not_found.detail.as_deref(), Some("account acc-1 not found"));

    let invalid = Problem::from(&ServiceError::invalid("account id", "bad shape"));
    assert_eq!(invalid.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(invalid.type_uri, "https://fishpond.dev/problems/invalid");

    let conflict = Problem::from(&ServiceError::conflict(
        "auth session",
        "ses-1",
        "duplicate live session",
    ));
    assert_eq!(conflict.status(), StatusCode::CONFLICT);
    assert_eq!(conflict.type_uri, "https://fishpond.dev/problems/conflict");
    assert_eq!(
        conflict.detail.as_deref(),
        Some("auth session ses-1: duplicate live session")
    );

    let reauth = Problem::from(&ServiceError::ReauthRequired {
        session: "ses-2".to_owned(),
        pinned: 3,
        current: 4,
    });
    assert_eq!(reauth.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(
        reauth.type_uri,
        "https://fishpond.dev/problems/reauth-required"
    );
    // Generations are fencing metadata, not secrets — but the detail
    // names only the session, keeping the body minimal.
    assert_eq!(
        reauth.detail.as_deref(),
        Some("session ses-2 must be re-authorized")
    );

    let storage = Problem::from(&ServiceError::Storage(
        fishpond_storage::StorageError::Invalid {
            what: "internal",
            reason: "sql said no".to_owned(),
        },
    ));
    assert_eq!(storage.status(), StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(storage.type_uri, "https://fishpond.dev/problems/storage");
    // Internal failures stay generic: no SQL, no reason text leaks.
    assert_eq!(storage.detail, None);
}

#[test]
fn problem_documents_serialize_rfc9457() {
    let problem = Problem::from(&ServiceError::not_found("account", "acc-1"));
    let json = serde_json::to_value(&problem).expect("serialize");
    assert_eq!(json["type"], "https://fishpond.dev/problems/not-found");
    assert_eq!(json["title"], "not found");
    assert_eq!(json["status"], 404);
    assert_eq!(json["detail"], "account acc-1 not found");
}

#[tokio::test]
async fn missing_authorization_is_rejected() {
    let state = test_state();
    let mut parts = parts_with_auth(None).await;
    let rejection = Caller::from_request_parts(&mut parts, &state)
        .await
        .expect_err("no header");
    assert_eq!(rejection.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(
        rejection.type_uri,
        "https://fishpond.dev/problems/unauthorized"
    );
    assert!(
        rejection
            .detail
            .expect("detail")
            .contains("missing Authorization")
    );
}

#[tokio::test]
async fn non_bearer_authorization_is_rejected() {
    let state = test_state();
    let mut parts = parts_with_auth(Some("Basic dXNlcjpwYXNz")).await;
    let rejection = Caller::from_request_parts(&mut parts, &state)
        .await
        .expect_err("wrong scheme");
    assert_eq!(rejection.status(), StatusCode::UNAUTHORIZED);
    assert!(
        rejection.detail.expect("detail").contains("Bearer"),
        "the detail names the expected scheme"
    );
}

#[tokio::test]
async fn unknown_key_is_rejected_without_echo() {
    let state = test_state();
    let mut parts = parts_with_auth(Some("Bearer key-nope")).await;
    let rejection = Caller::from_request_parts(&mut parts, &state)
        .await
        .expect_err("unknown key");
    assert_eq!(rejection.status(), StatusCode::UNAUTHORIZED);
    let text = serde_json::to_string(&rejection).expect("serialize");
    assert!(
        !text.contains("key-nope"),
        "the presented key is never echoed back: {text}"
    );
}

#[tokio::test]
async fn valid_key_resolves_tenant_and_scopes() {
    let state = test_state();
    let mut parts = parts_with_auth(Some("Bearer key-good")).await;
    let caller = Caller::from_request_parts(&mut parts, &state)
        .await
        .expect("valid key");
    assert_eq!(caller.tenant.as_str(), "t-tenant-a");
    assert!(caller.has("accounts:read"));
    assert!(caller.has("events:read"));
    assert!(!caller.has("accounts:write"), "ungranted scope denied");
}
