//! F026 acceptance: one mock end-to-end flow over the REST face —
//! authorize → message → catalog → orders (closed) → operations →
//! webhook — composing the real services, the capability router, the
//! mock provider, the event log and webhook fan-out with an in-process
//! transport (the real HTTP client stays behind the F032+ red line).
//!
//! The mock's seeded world lives in the fixed tenant `t-default`, so
//! this suite upserts that tenant idempotently and makes every
//! client-chosen id unique per run; the account registration accepts
//! the rerun duplicate as a conflict.
//!
//! Skips without a database, like every other DB-backed suite.

#[allow(dead_code)]
mod common;

use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::{gated, ts, unique_entity, unique_token};
use fishpond_domain::{AccountId, ConversationId, EventEnvelope, EventId, TenantId, UnixMillis};
use fishpond_events::{TransportOutcome, WebhookService, WebhookTransport};
use fishpond_provider_mock::{MockPlatform, MockProvider, SEED_ACCOUNT, SEED_CONVERSATION};
use fishpond_provider_sdk::{CallContext, Capability, ProtocolVersion, SendMessageRequest};
use fishpond_router::{BreakerPolicy, CapabilityRouter, RouteError};
use fishpond_server::http::auth::{RestKeyRecord, RestKeys};
use fishpond_server::{AppState, build_router};
use fishpond_storage::{
    CredentialCipher, MasterKey, NewCredential, NewEvent, ResolvedKey, SCOPE_ALL, SecretBox,
};
use http_body_util::BodyExt;
use tower::ServiceExt;

/// One recorded webhook POST (headers include the F018 signature).
#[derive(Debug, Clone)]
struct RecordedCall {
    url: String,
    headers: Vec<(String, String)>,
    body: String,
}

/// In-process webhook transport: records and accepts.
struct RecordingTransport {
    calls: Mutex<Vec<RecordedCall>>,
}

#[async_trait::async_trait]
impl WebhookTransport for RecordingTransport {
    async fn deliver(
        &self,
        url: &str,
        headers: &[(&'static str, String)],
        body: &str,
    ) -> TransportOutcome {
        self.calls
            .lock()
            .expect("transport lock")
            .push(RecordedCall {
                url: url.to_owned(),
                headers: headers
                    .iter()
                    .map(|(name, value)| ((*name).to_owned(), value.clone()))
                    .collect(),
                body: body.to_owned(),
            });
        TransportOutcome::Success
    }
}

fn flow_key(scopes: &[&str]) -> ResolvedKey {
    ResolvedKey {
        tenant_id: TenantId::parse("t-default").expect("seed tenant id"),
        role: "flow".to_owned(),
        scopes: scopes.iter().map(|s| (*s).to_owned()).collect(),
        expires_at: UnixMillis::from_millis(i64::MAX).expect("far future"),
    }
}

async fn call_json(
    router: &Router,
    method: &str,
    uri: &str,
    body: Option<serde_json::Value>,
    idempotency_key: Option<&str>,
) -> (StatusCode, serde_json::Value) {
    let mut builder = Request::builder()
        .method(method)
        .uri(uri)
        .header("authorization", "Bearer key-f026-flow");
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

/// Awaits one routed provider call through the breaker-settling
/// executor (route is a sync gate; execute is the async call).
macro_rules! routed {
    ($router:expr, $key:expr, $capability:expr, $ctx:expr, $now:expr, $call:expr) => {
        match $router.route($key, "mock", $capability, &[ProtocolVersion::V1_0], $now) {
            Ok(plan) => $router.execute(&plan, $ctx, $now, $call).await,
            Err(err) => Err(err),
        }
    };
}

#[tokio::test]
async fn mock_end_to_end_flow() {
    let Some(store) = gated().await else {
        eprintln!("skip: no database");
        return;
    };
    let token = unique_token("f026");
    let now_ms = common::ts(1_000).as_millis();

    // Fixed tenant (the mock's seeded world), idempotent upsert; one
    // master-key generation registered per run so the cipher's KEK is
    // a known row.
    let tenant = TenantId::parse("t-default").expect("seed tenant id");
    store
        .upsert_tenant(&common::tenant_record(&tenant, "t-default"))
        .await
        .expect("tenant upsert");
    let cred_version = unique_version_seed(&format!("{token}-cred"));
    store
        .register_master_key(cred_version, ts(500))
        .await
        .expect("master key");
    let cipher = CredentialCipher::new(MasterKey::generate(), cred_version);
    let hook_version = unique_version_seed(&format!("{token}-hook"));
    store
        .register_master_key(hook_version, ts(500))
        .await
        .expect("master key");
    let hook_cipher = CredentialCipher::new(MasterKey::generate(), hook_version);

    // The provider behind the router and the REST face over the same
    // store.
    let platform = MockPlatform::seeded();
    let mut upstream = CapabilityRouter::new(BreakerPolicy::new(3, 100).expect("valid policy"));
    upstream.register("mock", Arc::new(MockProvider::new(platform.clone())));

    let mut keys = RestKeys::new();
    keys.insert(
        "key-f026-flow".to_owned(),
        RestKeyRecord {
            tenant_id: tenant.clone(),
            scopes: vec![
                "accounts:read".to_owned(),
                "accounts:write".to_owned(),
                "sessions:read".to_owned(),
                "sessions:write".to_owned(),
                "conversations:read".to_owned(),
                "conversations:write".to_owned(),
                "catalog:read".to_owned(),
                "operations:read".to_owned(),
                "operations:write".to_owned(),
                "events:read".to_owned(),
            ],
        },
    );
    let http = build_router(AppState::new(store.clone(), keys));

    // ── 授权 authorize ──────────────────────────────────────────
    // Register the seeded account (reruns hit the unique conflict).
    let (status, created) = call_json(
        &http,
        "POST",
        "/accounts",
        Some(serde_json::json!({ "id": SEED_ACCOUNT, "platform": "mock" })),
        None,
    )
    .await;
    assert!(
        status == StatusCode::CREATED || status == StatusCode::CONFLICT,
        "{status} {created}"
    );

    // Start a session and walk it to AWAITING_CONFIRMATION.
    let session_id = unique_entity("ses", &token);
    let (status, started) = call_json(
        &http,
        "POST",
        "/sessions",
        Some(serde_json::json!({ "id": session_id, "account_id": SEED_ACCOUNT })),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{started}");
    assert_eq!(started["state"], "PENDING");
    let (status, confirmed) = call_json(
        &http,
        "POST",
        &format!("/sessions/{session_id}/transition"),
        Some(serde_json::json!({ "action": "confirm", "note": "flow" })),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{confirmed}");
    assert_eq!(confirmed["state"], "AWAITING_CONFIRMATION");

    // The provider-side login through the router (Authorize gate:
    // accounts:read), then the credential lands in the store and the
    // session is pinned to it.
    let key = flow_key(&["accounts:read", "conversations:write", "catalog:read"]);
    let ctx = CallContext::read(tenant.clone(), AccountId::parse(SEED_ACCOUNT).expect("id"));
    let outcome = routed!(
        &upstream,
        &key,
        Capability::Authorize,
        &ctx,
        now_ms,
        |p, c| Box::pin(p.authorize(c))
    )
    .expect("provider authorize");
    assert!(outcome.session_ref.starts_with("mock-session-"));

    let credential_id = unique_entity("cred", &token);
    store
        .store_credential(NewCredential {
            id: &credential_id,
            tenant: &tenant,
            platform: "mock",
            kind: "api_token",
            cipher: &cipher,
            secret: SecretBox::new("flow-credential-secret"),
            created_at: ts(2_000),
        })
        .await
        .expect("credential store");
    let (status, authorized) = call_json(
        &http,
        "POST",
        &format!("/sessions/{session_id}/transition"),
        Some(serde_json::json!({
            "action": "authorize",
            "credential_id": credential_id,
            "credential_version": 1,
        })),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{authorized}");
    assert_eq!(authorized["state"], "AUTHORIZED");
    assert_eq!(authorized["credential_id"], credential_id.as_str());

    // ── Webhook 订阅（在发消息前，扇出才接得到） ────────────────
    // The webhook phase runs in its own per-run tenant: fan-out is
    // tenant-scoped, so the fixed mock tenant would pile up
    // subscriptions (and due deliveries) across reruns.
    let hook_tenant = common::unique_tenant("f026-hooks");
    store
        .upsert_tenant(&common::tenant_record(&hook_tenant, "f026-hooks"))
        .await
        .expect("hook tenant insert");
    let hooks = WebhookService::new(store.clone(), hook_cipher);
    let sub_id = unique_entity("sub", &token);
    hooks
        .subscribe(
            &hook_tenant,
            &sub_id,
            "https://hooks.example.com/fishpond",
            SecretBox::new("whsec-flow"),
            Some("message.sent"),
            ts(2_500),
        )
        .await
        .expect("subscribe");

    // ── 消息 message ────────────────────────────────────────────
    let text = format!("flow ping {token}");
    let send = SendMessageRequest {
        conversation: ConversationId::parse(SEED_CONVERSATION).expect("seed conversation id"),
        text: text.clone(),
    };
    let send_key = unique_entity("idem", &token);
    let ctx = CallContext::write(
        tenant.clone(),
        AccountId::parse(SEED_ACCOUNT).expect("id"),
        send_key,
    )
    .expect("write context");
    let outcome = routed!(
        &upstream,
        &key,
        Capability::SendMessage,
        &ctx,
        now_ms,
        |p, c| Box::pin(p.send_message(c, send.clone()))
    )
    .expect("send_message");
    assert!(!outcome.deduplicated);

    // The mock emitted message.sent; publish it (with a store-unique
    // id — the platform's counter restarts per process) into the log.
    let emitted: Vec<EventEnvelope> = platform
        .take_events()
        .into_iter()
        .map(|envelope| EventEnvelope {
            id: EventId::parse(unique_entity("evt", &token)).expect("event id"),
            ..envelope
        })
        .collect();
    assert!(!emitted.is_empty(), "the mock must emit message.sent");
    let rows: Vec<NewEvent<'_>> = emitted
        .iter()
        .map(|envelope| NewEvent {
            id: &envelope.id,
            event_type: envelope.event_type.as_str(),
            account_id: envelope.account_id.as_ref(),
            occurred_at: envelope.occurred_at,
            payload: &envelope.payload,
        })
        .collect();
    let stored = store
        .append_events(&tenant, &rows)
        .await
        .expect("events append");
    let first = &stored[0];

    // The log page shows the event (payload arrives as JSON). A wide
    // page because earlier runs of this suite share the tenant's log.
    let (status, page) = call_json(&http, "GET", "/events?limit=200", None, None).await;
    assert_eq!(status, StatusCode::OK, "{page}");
    let page_text = serde_json::to_string(&page).expect("page json");
    assert!(
        page_text.contains(&token),
        "log carries this run's event: {page_text}"
    );

    // ── Webhook fan-out + delivery ──────────────────────────────
    let delivery_ids = hooks
        .enqueue(&hook_tenant, first, ts(3_000))
        .await
        .expect("fan out");
    assert_eq!(delivery_ids.len(), 1, "one matching subscription");
    let transport = Arc::new(RecordingTransport {
        calls: Mutex::new(Vec::new()),
    });
    let report = hooks
        .deliver_due(&hook_tenant, transport.as_ref(), ts(3_100), 60_000, 10)
        .await
        .expect("delivery pass");
    assert_eq!(report.delivered, 1, "{report:?}");
    let recorded = {
        let calls = transport.calls.lock().expect("transport lock");
        assert_eq!(calls.len(), 1);
        calls[0].clone()
    };
    assert_eq!(recorded.url, "https://hooks.example.com/fishpond");
    assert!(
        recorded
            .headers
            .iter()
            .any(|(name, _)| name == fishpond_events::SIGNATURE_HEADER),
        "the delivery is signed: {:?}",
        recorded.headers
    );
    assert!(
        recorded.body.contains(&token),
        "body carries the event: {}",
        recorded.body
    );

    // ── 商品 catalog ────────────────────────────────────────────
    let ctx = CallContext::read(tenant.clone(), AccountId::parse(SEED_ACCOUNT).expect("id"));
    let products = routed!(
        &upstream,
        &key,
        Capability::ReadCatalog,
        &ctx,
        now_ms,
        |p, c| Box::pin(p.list_products(c))
    )
    .expect("list_products");
    assert!(!products.is_empty(), "the seeded catalog is reachable");

    // ── 订单 orders：无 scope 映射 → 对任意 key 不可路由 ────────
    let all_scope = flow_key(&SCOPE_ALL);
    let denied = upstream
        .route(
            &all_scope,
            "mock",
            Capability::ManageOrders,
            &[ProtocolVersion::V1_0],
            now_ms,
        )
        .expect_err("orders stay closed");
    assert!(
        matches!(denied, RouteError::NotRoutable { .. }),
        "orders:manage is not routable, not scope-denied: {denied:?}"
    );

    // ── 操作 operations（REST 幂等生命周期） ────────────────────
    let op_id = unique_entity("op", &token);
    let op_key = unique_entity("opidem", &token);
    let (status, created) = call_json(
        &http,
        "POST",
        "/operations",
        Some(serde_json::json!({ "id": op_id, "kind": "send_message" })),
        Some(op_key.as_str()),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let worker = unique_entity("wkr", &token);
    let attempt = unique_entity("att", &token);
    let (status, claimed) = call_json(
        &http,
        "POST",
        &format!("/operations/{op_id}/claim"),
        Some(serde_json::json!({
            "worker_id": worker,
            "attempt_id": attempt,
            "deadline_at": now_ms + 60_000,
        })),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{claimed}");
    assert_eq!(claimed["operation"]["state"], "RUNNING");
    let (status, closed) = call_json(
        &http,
        "POST",
        &format!("/operations/{op_id}/attempts/{attempt}/close"),
        Some(serde_json::json!({ "worker_id": worker, "result": "succeeded" })),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{closed}");
    assert_eq!(closed["state"], "SUCCEEDED");

    // ── SSE：重放整个日志（Last-Event-ID: 0）含本轮事件 ─────────
    let request = Request::builder()
        .method("GET")
        .uri(format!(
            "/events/stream?consumer={}",
            unique_entity("sse", &token)
        ))
        .header("authorization", "Bearer key-f026-flow")
        .header("last-event-id", "0")
        .body(Body::empty())
        .expect("request");
    let response = http.oneshot(request).await.expect("response");
    assert_eq!(response.status(), StatusCode::OK);
    let stream = String::from_utf8(
        BodyExt::collect(response.into_body())
            .await
            .expect("body")
            .to_bytes()
            .to_vec(),
    )
    .expect("utf-8");
    assert!(stream.contains("event: message.sent"), "{stream}");
    assert!(
        stream.contains(&token),
        "the stream replays this run: {stream}"
    );
    assert!(stream.contains("event: caught-up"), "{stream}");
}

/// A master-key version unique to this run (the table is global).
fn unique_version_seed(token: &str) -> i32 {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock is after the epoch")
        .as_nanos();
    let mut hash: u64 = 14695981039346656037;
    for byte in format!("{token}-{nanos}").bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(1099511628211);
    }
    1_000_000 + (hash % 1_000_000_000) as i32
}
