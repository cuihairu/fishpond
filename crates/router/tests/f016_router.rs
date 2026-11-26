//! F016 acceptance: the capability router end-to-end over the mock
//! provider — the five gates in precedence order, circuit opening on
//! consecutive upstream faults, caller faults not counting, probe
//! recovery. No database: routing is in-memory by design (§7.3: Mock
//! is the only CI target; real engines are F032+, authorization-gated).

use std::sync::Arc;

use fishpond_domain::{AccountId, TenantId, UnixMillis};
use fishpond_provider_mock::{MockFault, MockProvider};
use fishpond_provider_sdk::{
    CallContext, Capability, ProtocolVersion, Provider, ProviderError, ProviderManifest,
    ProviderResult,
};
use fishpond_router::{BreakerPolicy, CapabilityRouter, RouteError};
use fishpond_storage::ResolvedKey;

fn key(scopes: &[&str]) -> ResolvedKey {
    ResolvedKey {
        tenant_id: TenantId::parse(fishpond_provider_mock::SEED_TENANT).expect("seed tenant id"),
        role: "operator".to_owned(),
        scopes: scopes.iter().map(|s| (*s).to_owned()).collect(),
        expires_at: UnixMillis::from_millis(i64::MAX).expect("far future"),
    }
}

fn router_with_mock() -> (CapabilityRouter, Arc<MockProvider>) {
    let provider = Arc::new(MockProvider::seeded());
    let mut router = CapabilityRouter::new(BreakerPolicy::new(3, 100).expect("valid policy"));
    router.register("mock", Arc::clone(&provider) as Arc<dyn Provider>);
    (router, provider)
}

/// A minimal provider whose manifest declares only `catalog:read` —
/// for the capability-support gate.
struct CatalogOnly;

#[async_trait::async_trait]
impl Provider for CatalogOnly {
    fn manifest(&self) -> &ProviderManifest {
        static MANIFEST: std::sync::OnceLock<ProviderManifest> = std::sync::OnceLock::new();
        MANIFEST.get_or_init(|| ProviderManifest {
            provider_id: "catalog-only".to_owned(),
            display_name: "Catalog Only".to_owned(),
            provider_version: "0.1.0".to_owned(),
            engine: fishpond_provider_sdk::EngineKind::Api,
            protocol_versions: vec![ProtocolVersion::V1_0],
            capabilities: vec![Capability::ReadCatalog],
        })
    }

    async fn authorize(
        &self,
        _ctx: &CallContext,
    ) -> ProviderResult<fishpond_provider_sdk::AuthorizeOutcome> {
        Err(ProviderError::new(
            fishpond_provider_sdk::ErrorClass::Unsupported,
            "not_implemented",
            "stub",
        ))
    }

    async fn send_message(
        &self,
        _ctx: &CallContext,
        _request: fishpond_provider_sdk::SendMessageRequest,
    ) -> ProviderResult<fishpond_provider_sdk::SendMessageOutcome> {
        Err(ProviderError::new(
            fishpond_provider_sdk::ErrorClass::Unsupported,
            "not_implemented",
            "stub",
        ))
    }

    async fn list_conversations(
        &self,
        _ctx: &CallContext,
    ) -> ProviderResult<Vec<fishpond_domain::Conversation>> {
        Err(ProviderError::new(
            fishpond_provider_sdk::ErrorClass::Unsupported,
            "not_implemented",
            "stub",
        ))
    }

    async fn list_messages(
        &self,
        _ctx: &CallContext,
        _conversation: &fishpond_domain::ConversationId,
    ) -> ProviderResult<Vec<fishpond_domain::Message>> {
        Err(ProviderError::new(
            fishpond_provider_sdk::ErrorClass::Unsupported,
            "not_implemented",
            "stub",
        ))
    }

    async fn list_products(
        &self,
        _ctx: &CallContext,
    ) -> ProviderResult<Vec<fishpond_domain::Product>> {
        Err(ProviderError::new(
            fishpond_provider_sdk::ErrorClass::Unsupported,
            "not_implemented",
            "stub",
        ))
    }
}

#[tokio::test]
async fn a_routed_send_goes_through_every_gate_and_executes() {
    let (router, provider) = router_with_mock();
    // Authorize rides the same precedence: accounts:read + write scope.
    let full = key(&["accounts:read", "conversations:write"]);

    // Route (no upstream touched) then execute against the mock.
    let plan = router
        .route(
            &full,
            "mock",
            Capability::SendMessage,
            &[ProtocolVersion::V1_0, ProtocolVersion::V1_1],
            0,
        )
        .expect("route send_message");
    assert_eq!(
        plan.protocol,
        ProtocolVersion::V1_1,
        "highest shared version wins"
    );
    assert_eq!(plan.breaker_key, "t-default/mock");

    let ctx = CallContext::write(
        full.tenant_id.clone(),
        AccountId::parse(fishpond_provider_mock::SEED_ACCOUNT).expect("account id"),
        "send-once",
    )
    .expect("write context");
    // The mock gates every face behind a live session: authorize first
    // (the contract suite's own order), through the same router.
    let auth_plan = router
        .route(
            &full,
            "mock",
            Capability::Authorize,
            &[ProtocolVersion::V1_0],
            0,
        )
        .expect("route authorize");
    router
        .execute(&auth_plan, &ctx, 0, |p, c| Box::pin(p.authorize(c)))
        .await
        .expect("authorize");
    let request = fishpond_provider_sdk::SendMessageRequest {
        conversation: fishpond_domain::ConversationId::parse(
            fishpond_provider_mock::SEED_CONVERSATION,
        )
        .expect("seed conversation id"),
        text: "hello through the router".to_owned(),
    };
    let outcome = router
        .execute(&plan, &ctx, 0, |p, c| {
            Box::pin(p.send_message(c, request.clone()))
        })
        .await
        .expect("send");
    assert!(!outcome.deduplicated);
    assert_eq!(outcome.message.text, "hello through the router");
    assert_eq!(provider.platform().event_count(), 1, "the send landed");

    // The success settled the circuit: still closed, streak reset.
    assert_eq!(
        router.breaker().state("t-default/mock"),
        fishpond_router::BreakerState::Closed
    );
}

#[tokio::test]
async fn gates_fail_closed_in_precedence_order() {
    let (router, _provider) = router_with_mock();
    let offer = [ProtocolVersion::V1_0];

    // Gate 1 (scope): a key without conversations:write is denied
    // before topology is consulted — an unknown platform with the
    // scope still loses to the scope gate.
    let scoped_out = key(&["catalog:read"]);
    let err = router
        .route(
            &scoped_out,
            "nonexistent-platform",
            Capability::SendMessage,
            &offer,
            0,
        )
        .expect_err("scope first");
    assert!(
        matches!(err, RouteError::ScopeDenied { scope } if scope == "conversations:write"),
        "scope gate wins over platform gate: {err}"
    );

    // Gate 2 (routability): every frozen scope still cannot route
    // orders:manage — privileges never conjure a missing scope.
    let all = key(&fishpond_storage::SCOPE_ALL);
    let err = router
        .route(&all, "mock", Capability::ManageOrders, &offer, 0)
        .expect_err("unroutable");
    assert!(matches!(err, RouteError::NotRoutable { .. }), "{err}");

    // Gate 3 (platform): scoped but unregistered platform.
    let err = router
        .route(
            &all,
            "nonexistent-platform",
            Capability::SendMessage,
            &offer,
            0,
        )
        .expect_err("no provider");
    assert!(
        matches!(err, RouteError::NoProviderForPlatform { .. }),
        "{err}"
    );

    // Gate 4a (capability support): a registered provider whose
    // manifest lacks the capability.
    let mut narrow = CapabilityRouter::new(BreakerPolicy::new(3, 100).expect("valid policy"));
    narrow.register("catalog-only", Arc::new(CatalogOnly));
    let err = narrow
        .route(
            &all,
            "catalog-only",
            Capability::ReadConversations,
            &offer,
            0,
        )
        .expect_err("capability missing");
    let RouteError::CapabilityNotSupported {
        provider,
        capability,
    } = err
    else {
        panic!("expected CapabilityNotSupported, got {err}");
    };
    assert_eq!(provider, "catalog-only");
    assert_eq!(capability, Capability::ReadConversations);

    // Gate 4b (protocol): a disjoint version offer is an upstream
    // Unsupported, not a route.
    let err = router
        .route(
            &all,
            "mock",
            Capability::SendMessage,
            &[ProtocolVersion::V2_0],
            0,
        )
        .expect_err("disjoint offer");
    let RouteError::Upstream(e) = err else {
        panic!("expected Upstream, got {err}");
    };
    assert_eq!(e.class, fishpond_provider_sdk::ErrorClass::Unsupported);
    assert!(e.message.contains("no shared protocol version"), "{e}");
}

#[tokio::test]
async fn upstream_faults_open_the_circuit_and_probes_heal_it() {
    let (router, provider) = router_with_mock();
    let full = key(&fishpond_storage::SCOPE_ALL);

    // Live session first: the fault under test is the write path, not
    // the auth gate.
    let setup_ctx = CallContext::write(
        full.tenant_id.clone(),
        AccountId::parse(fishpond_provider_mock::SEED_ACCOUNT).expect("account id"),
        "authorize-before-circuit",
    )
    .expect("write context");
    let auth_plan = router
        .route(
            &full,
            "mock",
            Capability::Authorize,
            &[ProtocolVersion::V1_0],
            0,
        )
        .expect("route authorize");
    router
        .execute(&auth_plan, &setup_ctx, 0, |p, c| Box::pin(p.authorize(c)))
        .await
        .expect("authorize");

    // Trip the mock's write rejection: every send fails Transient.
    provider.set_fault(MockFault::WriteRejection, true);
    let plan = router
        .route(
            &full,
            "mock",
            Capability::SendMessage,
            &[ProtocolVersion::V1_0],
            0,
        )
        .expect("route");
    let ctx = CallContext::write(
        full.tenant_id.clone(),
        AccountId::parse(fishpond_provider_mock::SEED_ACCOUNT).expect("account id"),
        "circuit-key",
    )
    .expect("write context");
    let request = fishpond_provider_sdk::SendMessageRequest {
        conversation: fishpond_domain::ConversationId::parse(
            fishpond_provider_mock::SEED_CONVERSATION,
        )
        .expect("seed conversation id"),
        text: "will fail".to_owned(),
    };

    // Two consecutive upstream failures stay closed (threshold 3);
    // each returns the provider error wrapped as Upstream.
    for now in [1, 2] {
        let err = router
            .execute(&plan, &ctx, now, |p, c| {
                Box::pin(p.send_message(c, request.clone()))
            })
            .await
            .expect_err("rejected upstream");
        let RouteError::Upstream(e) = err else {
            panic!("expected Upstream, got {err}");
        };
        assert_eq!(e.class, fishpond_provider_sdk::ErrorClass::Transient);
        assert_eq!(
            router.breaker().state("t-default/mock"),
            fishpond_router::BreakerState::Closed,
            "below threshold at attempt {now}"
        );
    }

    // The third failure opens the circuit.
    router
        .execute(&plan, &ctx, 3, |p, c| {
            Box::pin(p.send_message(c, request.clone()))
        })
        .await
        .expect_err("third failure");
    assert_eq!(
        router.breaker().state("t-default/mock"),
        fishpond_router::BreakerState::Open
    );

    // Clear the fault: calls are still short-circuited while open —
    // the mock receives nothing (CircuitOpen is not an upstream call).
    provider.set_fault(MockFault::WriteRejection, false);
    let err = router
        .route(
            &full,
            "mock",
            Capability::SendMessage,
            &[ProtocolVersion::V1_0],
            50,
        )
        .expect_err("circuit open at 50");
    let RouteError::CircuitOpen {
        key,
        retry_after_ms,
    } = err
    else {
        panic!("expected CircuitOpen, got {err}");
    };
    assert_eq!(key, "t-default/mock");
    assert_eq!(retry_after_ms, 53, "opened at 3, cooldown 100");
    assert_eq!(
        provider.platform().event_count(),
        0,
        "no call left the router"
    );

    // After the cooldown, the route half-opens and the probe succeeds.
    let probe_plan = router
        .route(
            &full,
            "mock",
            Capability::SendMessage,
            &[ProtocolVersion::V1_0],
            103,
        )
        .expect("probe allowed after cooldown");
    let sent = router
        .execute(&probe_plan, &ctx, 103, |p, c| {
            Box::pin(
                p.send_message(
                    c,
                    fishpond_provider_sdk::SendMessageRequest {
                        conversation: fishpond_domain::ConversationId::parse(
                            fishpond_provider_mock::SEED_CONVERSATION,
                        )
                        .expect("seed conversation id"),
                        text: "probe".to_owned(),
                    },
                ),
            )
        })
        .await
        .expect("probe succeeds");
    assert_eq!(sent.message.text, "probe");
    assert_eq!(
        router.breaker().state("t-default/mock"),
        fishpond_router::BreakerState::Closed,
        "probe success re-closes"
    );
}

#[tokio::test]
async fn caller_faults_never_trip_the_circuit() {
    let (router, _provider) = router_with_mock();
    let full = key(&fishpond_storage::SCOPE_ALL);
    let plan = router
        .route(
            &full,
            "mock",
            Capability::SendMessage,
            &[ProtocolVersion::V1_0],
            0,
        )
        .expect("route");
    let ctx = CallContext::write(
        full.tenant_id.clone(),
        AccountId::parse(fishpond_provider_mock::SEED_ACCOUNT).expect("account id"),
        "caller-fault-key",
    )
    .expect("write context");
    // Live session first, or every send would fail Auth instead of
    // exercising the caller-fault path.
    let auth_plan = router
        .route(
            &full,
            "mock",
            Capability::Authorize,
            &[ProtocolVersion::V1_0],
            0,
        )
        .expect("route authorize");
    router
        .execute(&auth_plan, &ctx, 0, |p, c| Box::pin(p.authorize(c)))
        .await
        .expect("authorize");

    // Empty text is the caller's fault (Invalid): run it five times,
    // far past the threshold — the circuit must stay closed.
    let request = fishpond_provider_sdk::SendMessageRequest {
        conversation: fishpond_domain::ConversationId::parse(
            fishpond_provider_mock::SEED_CONVERSATION,
        )
        .expect("seed conversation id"),
        text: "   ".to_owned(),
    };
    for now in 1..=5 {
        let err = router
            .execute(&plan, &ctx, now, |p, c| {
                Box::pin(p.send_message(c, request.clone()))
            })
            .await
            .expect_err("empty text rejected");
        let RouteError::Upstream(e) = err else {
            panic!("expected Upstream, got {err}");
        };
        assert_eq!(e.class, fishpond_provider_sdk::ErrorClass::Invalid);
    }
    assert_eq!(
        router.breaker().state("t-default/mock"),
        fishpond_router::BreakerState::Closed,
        "caller faults do not open circuits"
    );

    // And a well-formed send still goes straight through.
    let ok = router
        .execute(&plan, &ctx, 6, |p, c| {
            Box::pin(
                p.send_message(
                    c,
                    fishpond_provider_sdk::SendMessageRequest {
                        conversation: fishpond_domain::ConversationId::parse(
                            fishpond_provider_mock::SEED_CONVERSATION,
                        )
                        .expect("seed conversation id"),
                        text: "still healthy".to_owned(),
                    },
                ),
            )
        })
        .await
        .expect("healthy provider unaffected");
    assert_eq!(ok.message.text, "still healthy");
}

#[tokio::test]
async fn circuits_are_independent_per_tenant_and_platform() {
    let mut router = CapabilityRouter::new(BreakerPolicy::new(1, 100).expect("valid policy"));
    router.register(
        "mock",
        Arc::new(MockProvider::seeded()) as Arc<dyn Provider>,
    );
    let tenant_a = TenantId::parse("t-circuit-a").expect("tenant id");
    let tenant_b = TenantId::parse("t-circuit-b").expect("tenant id");

    // Trip tenant A's circuit with a manual failure record (threshold
    // 1); tenant B's identical platform stays closed.
    let breaker = router.breaker();
    breaker.record_failure(&format!("{tenant_a}/mock"), 1);
    assert_eq!(
        breaker.state(&format!("{tenant_a}/mock")),
        fishpond_router::BreakerState::Open
    );
    assert_eq!(
        breaker.state(&format!("{tenant_b}/mock")),
        fishpond_router::BreakerState::Closed
    );
}
