//! F031 initial load test (1,000 Mock sessions, engine plane): a
//! thousand concurrent sends through the API lane must all succeed
//! without tripping the breaker, and a browser-lane stampede must shed
//! load at the gate's pending bound instead of queueing without limit.
//! Pure — no database.

use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use fishpond_domain::{AccountId, ConversationId, TenantId, UnixMillis};
use fishpond_provider_mock::{
    MockPlatform, MockProvider, SEED_ACCOUNT, SEED_CONVERSATION, SEED_TENANT,
};
use fishpond_provider_sdk::{
    CallContext, Capability, EngineKind, ErrorClass, ProtocolVersion, Provider, ProviderError,
    ProviderManifest, ProviderResult, SendMessageRequest,
};
use fishpond_router::{BreakerPolicy, CapabilityRouter};
use fishpond_storage::ResolvedKey;
use fishpond_worker::{BackpressureGate, BrowserLane, EngineError, EngineRequest, EngineSelector};

fn flow_key(scopes: &[&str]) -> ResolvedKey {
    ResolvedKey {
        tenant_id: TenantId::parse(SEED_TENANT).expect("seed tenant id"),
        role: "operator".to_owned(),
        scopes: scopes.iter().map(|s| (*s).to_owned()).collect(),
        expires_at: UnixMillis::from_millis(i64::MAX).expect("far future"),
    }
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

/// Establishes the upstream platform session through the API lane (the
/// mock gates sends on an existing session).
async fn establish_session(platform: &MockPlatform) {
    let mut router = CapabilityRouter::new(BreakerPolicy::new(5, 100).expect("valid policy"));
    router.register("mock", Arc::new(MockProvider::new(platform.clone())));
    let key = flow_key(&["accounts:read"]);
    let ctx = CallContext::write(
        TenantId::parse(SEED_TENANT).expect("seed tenant id"),
        AccountId::parse(SEED_ACCOUNT).expect("seed account id"),
        "f031-establish",
    )
    .expect("write context");
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
const SESSIONS: usize = 1_000;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn thousand_concurrent_mock_sends_all_succeed() {
    let platform = MockPlatform::seeded();
    establish_session(&platform).await;
    let selected = Arc::new(selector(
        platform,
        None,
        BackpressureGate::new(64, 64).expect("bounds"),
    ));

    let mut joins = Vec::with_capacity(SESSIONS);
    for i in 0..SESSIONS {
        let selected = Arc::clone(&selected);
        joins.push(tokio::spawn(async move {
            let ctx = CallContext::write(
                TenantId::parse(SEED_TENANT).expect("seed tenant id"),
                AccountId::parse(SEED_ACCOUNT).expect("seed account id"),
                format!("f031-load-{i}"),
            )
            .expect("write context");
            let request = EngineRequest {
                key: &flow_key(&["conversations:write"]),
                platform: "mock",
                capability: Capability::SendMessage,
                offered: &[ProtocolVersion::V1_0],
                now: 1_000,
            };
            selected
                .execute(
                    request,
                    &ctx,
                    |provider, ctx| {
                        let text = format!("load-{i}");
                        Box::pin(async move {
                            provider
                                .send_message(
                                    ctx,
                                    SendMessageRequest {
                                        conversation: ConversationId::parse(SEED_CONVERSATION)
                                            .expect("seed conversation id"),
                                        text,
                                    },
                                )
                                .await?;
                            Ok(())
                        })
                    },
                    |_ctx| {
                        Box::pin(async {
                            Err(fishpond_worker::BrowserFault::CallerFault {
                                reason: "no browser lane is configured".to_owned(),
                            })
                        })
                    },
                )
                .await
        }));
    }

    let mut oks = 0usize;
    let mut failures = Vec::new();
    for join in joins {
        match join.await.expect("task completes") {
            Ok(()) => oks += 1,
            Err(error) => failures.push(error.to_string()),
        }
    }
    assert_eq!(oks, SESSIONS, "failures: {failures:?}");
}

/// An API provider that deliberately declares no send capability, so
/// every send routes straight to the browser lane and its gate — no
/// API attempts, no breaker interference, a deterministic partition.
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
        _conversation: &fishpond_domain::ConversationId,
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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn browser_stampede_sheds_load_at_the_pending_bound() {
    // The mock platform is irrelevant here (the API lane cannot serve
    // sends), but a registered router needs the platform entry.
    let mut router = CapabilityRouter::new(BreakerPolicy::new(5, 100).expect("valid policy"));
    router.register("mock", Arc::new(MockProvider::new(MockPlatform::seeded())));
    router.register("api-only", Arc::new(ApiOnly));
    let selected = Arc::new(EngineSelector::new(
        router,
        browser_lane(SEND_LANE),
        BackpressureGate::new(2, 2).expect("bounds"),
    ));

    // Freeze the stampede before releasing anyone: every task counts
    // in when it reaches the gate, admitted calls park on a latch, and
    // the test only opens the latch once the partition is settled —
    // 2 in flight + 2 queued + the rest already rejected. Without the
    // freeze, permits recycle mid-stampede and the split is a timing
    // race.
    let arrived = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let waiting = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let (open_tx, open_rx) = tokio::sync::watch::channel(false);

    let mut joins = Vec::with_capacity(50);
    for i in 0..50usize {
        let selected = Arc::clone(&selected);
        let arrived = Arc::clone(&arrived);
        let waiting = Arc::clone(&waiting);
        let open_rx = open_rx.clone();
        joins.push(tokio::spawn(async move {
            let ctx = CallContext::write(
                TenantId::parse(SEED_TENANT).expect("seed tenant id"),
                AccountId::parse(SEED_ACCOUNT).expect("seed account id"),
                format!("f031-stampede-{i}"),
            )
            .expect("write context");
            let request = EngineRequest {
                key: &flow_key(&["conversations:write"]),
                platform: "api-only",
                capability: Capability::SendMessage,
                offered: &[ProtocolVersion::V1_0],
                now: 1_000,
            };
            arrived.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            selected
                .execute(
                    request,
                    &ctx,
                    |_provider, _ctx| Box::pin(async { Err(unsupported("api lane is off")) }),
                    move |_ctx| {
                        let waiting = Arc::clone(&waiting);
                        let mut open_rx = open_rx.clone();
                        Box::pin(async move {
                            waiting.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                            let released = tokio::time::timeout(
                                std::time::Duration::from_secs(5),
                                open_rx.wait_for(|open| *open),
                            )
                            .await;
                            waiting.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
                            if released.is_err() {
                                return Err(fishpond_worker::BrowserFault::Transient {
                                    reason: "stampede latch timed out".to_owned(),
                                });
                            }
                            Ok(())
                        })
                    },
                )
                .await
        }));
    }

    // Wait for the settled partition: all 50 reached the gate, both
    // permits are held by parked callers, and both queue slots are
    // taken — so the other 46 are already rejected. Then let them all
    // through.
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let settled = arrived.load(std::sync::atomic::Ordering::SeqCst) == 50
            && waiting.load(std::sync::atomic::Ordering::SeqCst) == 2
            && selected.gate().pending() == 2;
        if settled || tokio::time::Instant::now() >= deadline {
            assert!(
                settled,
                "the stampede must settle at 2 in flight + 2 queued \
                 (arrived {}, waiting {}, pending {})",
                arrived.load(std::sync::atomic::Ordering::SeqCst),
                waiting.load(std::sync::atomic::Ordering::SeqCst),
                selected.gate().pending()
            );
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(1)).await;
    }
    open_tx.send(true).expect("watch receiver is alive");

    let mut served = 0usize;
    let mut shed = 0usize;
    let mut other = Vec::new();
    for join in joins {
        match join.await.expect("task completes") {
            Ok(()) => served += 1,
            Err(EngineError::Backpressured { capacity }) => {
                assert_eq!(capacity, 2);
                shed += 1;
            }
            Err(error) => other.push(error.to_string()),
        }
    }
    assert!(other.is_empty(), "nothing else may fail: {other:?}");
    assert_eq!(
        served + shed,
        50,
        "every call must land in exactly one bucket (served {served}, shed {shed})"
    );
    assert_eq!(
        served, 4,
        "exactly the 2 in-flight permits + 2 pending slots are admitted"
    );
    assert_eq!(shed, 46, "everything past the bound must be shed");
    // The gate must be fully drained afterwards.
    assert_eq!(selected.gate().pending(), 0);
}
