//! F024 acceptance (pure, no database): dual-engine selection (T09)
//! and the browser backpressure gate (T25). The API lane is the
//! router-gated default; the browser lane is the degraded path —
//! tried when the API engine cannot serve the capability or its
//! upstream faulted, never for authorization refusals or caller
//! faults. The gate bounds concurrent browser work and rejects past
//! the pending bound instead of queueing without limit.

use std::sync::{Arc, Mutex, OnceLock};

use async_trait::async_trait;
use fishpond_domain::{ConversationId, TenantId, UnixMillis};
use fishpond_provider_mock::{MockFault, MockPlatform, MockProvider, SEED_ACCOUNT, SEED_TENANT};
use fishpond_provider_sdk::{
    CallContext, Capability, EngineKind, ErrorClass, ProtocolVersion, Provider, ProviderError,
    ProviderManifest, ProviderResult, SendMessageRequest,
};
use fishpond_router::{BreakerPolicy, CapabilityRouter, RouteError};
use fishpond_storage::ResolvedKey;
use fishpond_worker::{
    BackpressureGate, BrowserFault, BrowserLane, EngineError, EngineRequest, EngineSelector,
};

/// A lane key the API/browser closures append to, proving attempt
/// order without touching real engines.
type Trace = Arc<Mutex<Vec<&'static str>>>;

fn trace() -> Trace {
    Arc::new(Mutex::new(Vec::new()))
}

fn flow_key(scopes: &[&str]) -> ResolvedKey {
    ResolvedKey {
        tenant_id: TenantId::parse(SEED_TENANT).expect("seed tenant id"),
        role: "operator".to_owned(),
        scopes: scopes.iter().map(|s| (*s).to_owned()).collect(),
        expires_at: UnixMillis::from_millis(i64::MAX).expect("far future"),
    }
}

fn write_ctx() -> CallContext {
    CallContext::write(
        TenantId::parse(SEED_TENANT).expect("seed tenant id"),
        fishpond_domain::AccountId::parse(SEED_ACCOUNT).expect("seed account id"),
        "flow-idem-key",
    )
    .expect("write context")
}

fn selector(
    platform: MockPlatform,
    lane: Option<BrowserLane>,
    gate: BackpressureGate,
) -> EngineSelector {
    let mut router = CapabilityRouter::new(BreakerPolicy::new(5, 100).expect("valid policy"));
    router.register("mock", Arc::new(MockProvider::new(platform)));
    EngineSelector::new(router, lane, gate)
}

fn browser_lane(capabilities: &'static [Capability]) -> Option<BrowserLane> {
    Some(BrowserLane {
        capabilities,
        available: true,
    })
}

fn gate(max_concurrent: usize, max_pending: usize) -> BackpressureGate {
    BackpressureGate::new(max_concurrent, max_pending).expect("valid bounds")
}

/// Establishes the upstream platform session through the API lane
/// itself (the mock gates sends on an existing session), so no
/// private mock internals are touched.
async fn establish_session(platform: &MockPlatform) {
    let mut router = CapabilityRouter::new(BreakerPolicy::new(5, 100).expect("valid policy"));
    router.register("mock", Arc::new(MockProvider::new(platform.clone())));
    let key = flow_key(&["accounts:read"]);
    let ctx = write_ctx();
    let plan = router
        .route(
            &key,
            "mock",
            Capability::Authorize,
            &[ProtocolVersion::V1_0],
            1_000,
        )
        .expect("authorize route");
    router
        .execute(&plan, &ctx, 1_000, |provider, ctx| {
            Box::pin(provider.authorize(ctx))
        })
        .await
        .expect("platform session established");
}

const SEND_LANE: &[Capability] = &[Capability::SendMessage];
const AUTHORIZE_LANE: &[Capability] = &[Capability::Authorize];

/// An API provider that deliberately declares one capability only,
/// so the selector must degrade to the browser lane for anything
/// else.
#[derive(Debug)]
struct ApiOnly;

static API_ONLY_MANIFEST: OnceLock<ProviderManifest> = OnceLock::new();

fn unsupported(code: &str) -> ProviderError {
    ProviderError::new(
        ErrorClass::Unsupported,
        code,
        "api-only stub serves nothing",
    )
}

#[async_trait]
impl Provider for ApiOnly {
    fn manifest(&self) -> &ProviderManifest {
        API_ONLY_MANIFEST.get_or_init(|| ProviderManifest {
            provider_id: "api-only".to_owned(),
            display_name: "API only stub".to_owned(),
            provider_version: "0.1.0".to_owned(),
            engine: EngineKind::Api,
            protocol_versions: vec![ProtocolVersion::V1_0],
            capabilities: vec![Capability::AccountState],
        })
    }

    async fn authorize(
        &self,
        _ctx: &CallContext,
    ) -> ProviderResult<fishpond_provider_sdk::AuthorizeOutcome> {
        Err(unsupported("authorize"))
    }

    async fn send_message(
        &self,
        _ctx: &CallContext,
        _request: SendMessageRequest,
    ) -> ProviderResult<fishpond_provider_sdk::SendMessageOutcome> {
        Err(unsupported("send_message"))
    }

    async fn list_conversations(
        &self,
        _ctx: &CallContext,
    ) -> ProviderResult<Vec<fishpond_domain::Conversation>> {
        Err(unsupported("list_conversations"))
    }

    async fn list_messages(
        &self,
        _ctx: &CallContext,
        _conversation: &ConversationId,
    ) -> ProviderResult<Vec<fishpond_domain::Message>> {
        Err(unsupported("list_messages"))
    }

    async fn list_products(
        &self,
        _ctx: &CallContext,
    ) -> ProviderResult<Vec<fishpond_domain::Product>> {
        Err(unsupported("list_products"))
    }
}

#[tokio::test]
async fn api_lane_is_preferred_when_it_serves() {
    let platform = MockPlatform::seeded();
    establish_session(&platform).await;
    let selected = selector(platform, browser_lane(SEND_LANE), gate(2, 2));
    let order = trace();

    let outcome = selected
        .execute(
            EngineRequest {
                key: &flow_key(&["conversations:write"]),
                platform: "mock",
                capability: Capability::SendMessage,
                offered: &[ProtocolVersion::V1_0],
                now: 1_000,
            },
            &write_ctx(),
            |provider, ctx| {
                let order = order.clone();
                Box::pin(async move {
                    order.lock().expect("trace lock").push("api");
                    let outcome = provider
                        .send_message(
                            ctx,
                            SendMessageRequest {
                                conversation: ConversationId::parse("conv-mock-1")
                                    .expect("seed conversation id"),
                                text: "via api".to_owned(),
                            },
                        )
                        .await?;
                    Ok(outcome.message.text)
                })
            },
            |_ctx| {
                let order = order.clone();
                Box::pin(async move {
                    order.lock().expect("trace lock").push("browser");
                    Ok("via page".to_owned())
                })
            },
        )
        .await
        .expect("api lane serves");
    assert_eq!(outcome, "via api");
    assert_eq!(*order.lock().expect("trace lock"), vec!["api"]);
}

#[tokio::test]
async fn upstream_fault_degrades_to_the_browser_lane() {
    let platform = MockPlatform::seeded();
    establish_session(&platform).await;
    platform.set_fault(MockFault::WriteRejection, true);
    let selected = selector(platform, browser_lane(SEND_LANE), gate(2, 2));
    let order = trace();

    let outcome = selected
        .execute(
            EngineRequest {
                key: &flow_key(&["conversations:write"]),
                platform: "mock",
                capability: Capability::SendMessage,
                offered: &[ProtocolVersion::V1_0],
                now: 1_000,
            },
            &write_ctx(),
            |provider, ctx| {
                let order = order.clone();
                Box::pin(async move {
                    order.lock().expect("trace lock").push("api");
                    let outcome = provider
                        .send_message(
                            ctx,
                            SendMessageRequest {
                                conversation: ConversationId::parse("conv-mock-1")
                                    .expect("seed conversation id"),
                                text: "via api".to_owned(),
                            },
                        )
                        .await?;
                    Ok(outcome.message.text)
                })
            },
            |_ctx| {
                let order = order.clone();
                Box::pin(async move {
                    order.lock().expect("trace lock").push("browser");
                    Ok("via page".to_owned())
                })
            },
        )
        .await
        .expect("browser lane catches the upstream fault");
    assert_eq!(outcome, "via page");
    assert_eq!(
        *order.lock().expect("trace lock"),
        vec!["api", "browser"],
        "the api lane is tried first, the browser lane is the degrade"
    );
}

#[tokio::test]
async fn caller_faults_pass_through_without_degrading() {
    let platform = MockPlatform::seeded();
    establish_session(&platform).await;
    platform.set_fault(MockFault::WriteRejection, true);
    let selected = selector(platform, browser_lane(SEND_LANE), gate(2, 2));
    let order = trace();

    let err = selected
        .execute(
            EngineRequest {
                key: &flow_key(&["conversations:write"]),
                platform: "mock",
                capability: Capability::SendMessage,
                offered: &[ProtocolVersion::V1_0],
                now: 1_000,
            },
            &write_ctx(),
            |provider, ctx| {
                let order = order.clone();
                Box::pin(async move {
                    order.lock().expect("trace lock").push("api");
                    let outcome = provider
                        .send_message(
                            ctx,
                            SendMessageRequest {
                                conversation: ConversationId::parse("conv-mock-1")
                                    .expect("seed conversation id"),
                                text: "via api".to_owned(),
                            },
                        )
                        .await?;
                    Ok(outcome.message.text)
                })
            },
            |_ctx| {
                let order = order.clone();
                Box::pin(async move {
                    order.lock().expect("trace lock").push("browser");
                    Err(BrowserFault::CallerFault {
                        reason: "selector matches nothing on the page".to_owned(),
                    })
                })
            },
        )
        .await
        .expect_err("both lanes failed");
    let EngineError::CallerFault { reason } = &err else {
        panic!("a caller fault is terminal truth about the operation: {err:?}");
    };
    assert!(
        reason.contains("selector matches nothing"),
        "caller fault passes through untouched: {err:?}"
    );
    assert_eq!(*order.lock().expect("trace lock"), vec!["api", "browser"]);
}

#[tokio::test]
async fn authorization_gates_fail_closed_without_degrading() {
    let platform = MockPlatform::seeded();
    let selected = selector(platform, browser_lane(SEND_LANE), gate(2, 2));
    let order = trace();

    // The key lacks conversations:write. The browser lane may serve
    // the capability — the selector must still refuse: authorization
    // problems are never degraded onto a page browser.
    let err = selected
        .execute(
            EngineRequest {
                key: &flow_key(&["catalog:read"]),
                platform: "mock",
                capability: Capability::SendMessage,
                offered: &[ProtocolVersion::V1_0],
                now: 1_000,
            },
            &write_ctx(),
            |_provider, _ctx| Box::pin(async move { unreachable!("api lane must not run") }),
            |_ctx| {
                let order = order.clone();
                Box::pin(async move {
                    order.lock().expect("trace lock").push("browser");
                    Ok("via page".to_owned())
                })
            },
        )
        .await
        .expect_err("scope denied");
    let EngineError::Route(RouteError::ScopeDenied { scope }) = &err else {
        panic!("fail closed with the router's class: {err:?}");
    };
    assert_eq!(*scope, "conversations:write");
    assert!(
        order.lock().expect("trace lock").is_empty(),
        "no lane ran for an authorization refusal"
    );
}

#[tokio::test]
async fn browser_lane_serves_when_the_api_lacks_the_capability() {
    let mut router = CapabilityRouter::new(BreakerPolicy::new(5, 100).expect("valid policy"));
    router.register("stub", Arc::new(ApiOnly));
    let selected = EngineSelector::new(router, browser_lane(AUTHORIZE_LANE), gate(2, 2));
    let order = trace();

    let outcome = selected
        .execute(
            EngineRequest {
                key: &flow_key(&["accounts:read"]),
                platform: "stub",
                capability: Capability::Authorize,
                offered: &[ProtocolVersion::V1_0],
                now: 1_000,
            },
            &write_ctx(),
            |_provider, _ctx| {
                Box::pin(async move { unreachable!("api lane lacks the capability") })
            },
            |_ctx| {
                let order = order.clone();
                Box::pin(async move {
                    order.lock().expect("trace lock").push("browser");
                    Ok("page session".to_owned())
                })
            },
        )
        .await
        .expect("the browser lane serves the capability");
    assert_eq!(outcome, "page session");
    assert_eq!(*order.lock().expect("trace lock"), vec!["browser"]);
}

#[tokio::test]
async fn no_lane_serves_means_no_engine() {
    let mut router = CapabilityRouter::new(BreakerPolicy::new(5, 100).expect("valid policy"));
    router.register("stub", Arc::new(ApiOnly));
    let selected = EngineSelector::new(router, browser_lane(AUTHORIZE_LANE), gate(2, 2));

    let err = selected
        .execute(
            EngineRequest {
                key: &flow_key(&["conversations:write"]),
                platform: "stub",
                capability: Capability::SendMessage,
                offered: &[ProtocolVersion::V1_0],
                now: 1_000,
            },
            &write_ctx(),
            |_provider, _ctx| {
                Box::pin(async move { unreachable!("api lane lacks the capability") })
            },
            |_ctx| Box::pin(async move { Ok("via page".to_owned()) }),
        )
        .await
        .expect_err("nobody serves SendMessage");
    let EngineError::NoEngine { capability } = &err else {
        panic!("the error names the orphan capability: {err:?}");
    };
    assert_eq!(*capability, Capability::SendMessage);
}

#[tokio::test]
async fn gate_rejects_past_the_pending_bound() {
    let gate = Arc::new(BackpressureGate::new(1, 1).expect("valid bounds"));
    let (release_tx, release_rx) = tokio::sync::oneshot::channel::<()>();

    // Slot 1: holds its permit until released.
    let holder = {
        let gate = gate.clone();
        tokio::spawn(async move {
            gate.run(async {
                let _ = release_rx.await;
                "held"
            })
            .await
        })
    };
    // Let the holder take the slot.
    for _ in 0..5 {
        tokio::task::yield_now().await;
    }

    // Pending 1: queued behind the holder.
    let queued = {
        let gate = gate.clone();
        tokio::spawn(async move { gate.run(async { "queued" }).await })
    };
    for _ in 0..5 {
        tokio::task::yield_now().await;
    }
    assert_eq!(gate.pending(), 1, "one caller waits for a slot");

    // Pending 2: past the bound — rejected now, naming the capacity.
    let rejected = gate.run(async { "too many" }).await;
    assert!(
        matches!(rejected, Err(EngineError::Backpressured { capacity }) if capacity == 1),
        "the bound names the capacity: {rejected:?}"
    );

    // Releasing the holder promotes the queued caller.
    release_tx.send(()).expect("release");
    assert_eq!(holder.await.expect("task").expect("held"), "held");
    assert_eq!(queued.await.expect("task").expect("queued"), "queued");
    assert_eq!(gate.pending(), 0, "the queue drains with the slots");
}

#[test]
fn gate_bounds_must_be_positive() {
    assert!(BackpressureGate::new(0, 1).is_none());
    assert!(BackpressureGate::new(1, 0).is_none());
    assert!(BackpressureGate::new(2, 3).is_some());
}
