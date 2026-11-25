//! The `provider_sdk::Provider` adapter over [`MockPlatform`].
//!
//! This is the seam the worker/router/scheduler drive in CI: identical
//! behaviour to a real provider by contract, with deterministic fault
//! injection.

use async_trait::async_trait;

use fishpond_domain::{Conversation, ConversationId, Message, Product};
use fishpond_provider_sdk::{
    AuthorizeOutcome, CallContext, Capability, EngineKind, ProtocolVersion, Provider,
    ProviderManifest, ProviderResult, SendMessageOutcome, SendMessageRequest,
};

use crate::platform::{MockFault, MockPlatform};

/// The mock provider: a [`MockPlatform`] behind the [`Provider`] seam.
#[derive(Clone)]
pub struct MockProvider {
    manifest: ProviderManifest,
    platform: MockPlatform,
}

impl MockProvider {
    /// Wraps a platform.
    pub fn new(platform: MockPlatform) -> Self {
        Self {
            manifest: ProviderManifest {
                provider_id: "mock".to_owned(),
                display_name: "Mock Platform".to_owned(),
                provider_version: "0.1.0".to_owned(),
                engine: EngineKind::Api,
                protocol_versions: vec![ProtocolVersion::V1_0, ProtocolVersion::V1_1],
                capabilities: vec![
                    Capability::AccountState,
                    Capability::Authorize,
                    Capability::ReadConversations,
                    Capability::SendMessage,
                    Capability::ReadCatalog,
                    Capability::StreamEvents,
                ],
            },
            platform,
        }
    }

    /// A provider over [`MockPlatform::seeded`], the contract fixture.
    pub fn seeded() -> Self {
        Self::new(MockPlatform::seeded())
    }

    /// The underlying platform (fault switches, event log).
    pub fn platform(&self) -> &MockPlatform {
        &self.platform
    }

    /// Turns a fault switch on or off.
    pub fn set_fault(&self, fault: MockFault, on: bool) {
        self.platform.set_fault(fault, on);
    }

    /// Whether a fault switch is engaged.
    pub fn fault(&self, fault: MockFault) -> bool {
        self.platform.fault(fault)
    }
}

#[async_trait]
impl Provider for MockProvider {
    fn manifest(&self) -> &ProviderManifest {
        &self.manifest
    }

    async fn authorize(&self, ctx: &CallContext) -> ProviderResult<AuthorizeOutcome> {
        self.platform.authorize_call(ctx)
    }

    async fn send_message(
        &self,
        ctx: &CallContext,
        request: SendMessageRequest,
    ) -> ProviderResult<SendMessageOutcome> {
        self.platform.send_call(ctx, request)
    }

    async fn list_conversations(&self, ctx: &CallContext) -> ProviderResult<Vec<Conversation>> {
        self.platform.conversations_call(ctx)
    }

    async fn list_messages(
        &self,
        ctx: &CallContext,
        conversation: &ConversationId,
    ) -> ProviderResult<Vec<Message>> {
        self.platform.messages_call(ctx, conversation)
    }

    async fn list_products(&self, ctx: &CallContext) -> ProviderResult<Vec<Product>> {
        self.platform.products_call(ctx)
    }
}

#[cfg(test)]
mod tests {
    use fishpond_domain::{AccountId, ConversationId, TenantId};
    use fishpond_provider_sdk::{ContractFixture, ErrorClass, run_contract_suite};

    use super::*;

    fn fixture() -> ContractFixture {
        ContractFixture {
            tenant: TenantId::parse(crate::platform::SEED_TENANT).unwrap(),
            account: AccountId::parse(crate::platform::SEED_ACCOUNT).unwrap(),
            conversation: ConversationId::parse(crate::platform::SEED_CONVERSATION).unwrap(),
        }
    }

    fn write_ctx(tenant: &TenantId, account: &AccountId, key: &str) -> CallContext {
        CallContext::write(tenant.clone(), account.clone(), key).unwrap()
    }

    #[tokio::test]
    async fn mock_passes_every_contract_check() {
        let provider = MockProvider::seeded();
        let report = run_contract_suite(&provider, &fixture()).await;
        report.assert_ok();
        assert_eq!(report.checks.len(), 9);
    }

    #[tokio::test]
    async fn authorization_fault_denies_then_recovers() {
        let provider = MockProvider::seeded();
        let ctx = write_ctx(&fixture().tenant, &fixture().account, "auth-fault");

        provider.set_fault(MockFault::AuthorizationDenied, true);
        let err = provider
            .authorize(&ctx)
            .await
            .expect_err("authorization fault must fail authorize");
        assert_eq!(err.class, ErrorClass::Auth);
        assert_eq!(err.code, "authorize_fault");
        assert!(err.retryable());

        provider.set_fault(MockFault::AuthorizationDenied, false);
        let outcome = provider.authorize(&ctx).await.expect("fault cleared");
        assert_eq!(outcome.session_ref, "mock-session-acc-mock");
    }

    #[tokio::test]
    async fn write_rejection_is_transient_and_emits_no_events() {
        let provider = MockProvider::seeded();
        let fixture = fixture();
        let ctx = write_ctx(&fixture.tenant, &fixture.account, "write-fault");
        provider.authorize(&ctx).await.expect("authorize first");

        provider.set_fault(MockFault::WriteRejection, true);
        let err = provider
            .send_message(
                &ctx,
                SendMessageRequest {
                    conversation: fixture.conversation.clone(),
                    text: "blocked".to_owned(),
                },
            )
            .await
            .expect_err("write fault must fail sends");
        assert_eq!(err.class, ErrorClass::Transient);
        assert_eq!(err.code, "write_rejected");
        assert!(err.retryable());
        assert_eq!(
            provider.platform().event_count(),
            0,
            "rejected writes emit nothing"
        );

        // Reads stay healthy under a write fault.
        provider
            .list_conversations(&ctx)
            .await
            .expect("reads survive write rejection");

        provider.set_fault(MockFault::WriteRejection, false);
        let outcome = provider
            .send_message(
                &ctx,
                SendMessageRequest {
                    conversation: fixture.conversation.clone(),
                    text: "blocked".to_owned(),
                },
            )
            .await
            .expect("send works once the fault clears");
        assert!(!outcome.deduplicated);
        assert_eq!(provider.platform().event_count(), 1);
    }

    #[tokio::test]
    async fn disconnect_fails_every_call_and_idempotency_survives() {
        let provider = MockProvider::seeded();
        let fixture = fixture();
        let ctx = write_ctx(&fixture.tenant, &fixture.account, "disconnect");
        provider.authorize(&ctx).await.expect("authorize first");
        let before = provider
            .send_message(
                &ctx,
                SendMessageRequest {
                    conversation: fixture.conversation.clone(),
                    text: "before outage".to_owned(),
                },
            )
            .await
            .expect("send before the outage");
        assert!(!before.deduplicated);

        provider.set_fault(MockFault::Disconnect, true);
        for attempt in [
            provider.authorize(&ctx).await.map(|_| ()),
            provider
                .send_message(
                    &ctx,
                    SendMessageRequest {
                        conversation: fixture.conversation.clone(),
                        text: "during outage".to_owned(),
                    },
                )
                .await
                .map(|_| ()),
            provider.list_conversations(&ctx).await.map(|_| ()),
            provider
                .list_messages(&ctx, &fixture.conversation)
                .await
                .map(|_| ()),
            provider.list_products(&ctx).await.map(|_| ()),
        ] {
            let err = attempt.expect_err("every call fails while disconnected");
            assert_eq!(err.class, ErrorClass::Timeout);
            assert_eq!(err.code, "connection_lost");
            assert!(err.retryable());
        }

        provider.set_fault(MockFault::Disconnect, false);
        let replay = provider
            .send_message(
                &ctx,
                SendMessageRequest {
                    conversation: fixture.conversation.clone(),
                    text: "before outage".to_owned(),
                },
            )
            .await
            .expect("reconnect restores the send path");
        assert!(replay.deduplicated, "replay after reconnect deduplicates");
        assert_eq!(replay.message.id, before.message.id);
    }

    #[tokio::test]
    async fn duplicate_event_delivered_twice_then_cleared() {
        let provider = MockProvider::seeded();
        let fixture = fixture();
        let ctx = write_ctx(&fixture.tenant, &fixture.account, "dup-1");
        provider.authorize(&ctx).await.expect("authorize first");
        provider.set_fault(MockFault::DuplicateNextEvent, true);

        provider
            .send_message(
                &ctx,
                SendMessageRequest {
                    conversation: fixture.conversation.clone(),
                    text: "emit me twice".to_owned(),
                },
            )
            .await
            .expect("send under the duplicate fault");
        let duplicated = provider.platform().take_events();
        assert_eq!(duplicated.len(), 2);
        assert_eq!(
            duplicated[0], duplicated[1],
            "the same envelope is redelivered"
        );
        assert!(
            !provider.fault(MockFault::DuplicateNextEvent),
            "one-shot switch cleared"
        );

        let ctx2 = write_ctx(&fixture.tenant, &fixture.account, "dup-2");
        provider
            .send_message(
                &ctx2,
                SendMessageRequest {
                    conversation: fixture.conversation.clone(),
                    text: "emit me once".to_owned(),
                },
            )
            .await
            .expect("subsequent sends are normal");
        let events = provider.platform().take_events();
        assert_eq!(events.len(), 1, "no duplicate the second time");

        // Downstream dedup collapses on event id.
        let mut all = Vec::new();
        all.extend(duplicated);
        all.extend(events);
        let mut seen = std::collections::BTreeSet::new();
        let unique = all
            .into_iter()
            .filter(|e| seen.insert(e.id.clone()))
            .count();
        assert_eq!(unique, 2, "two real events once deduplicated by id");
    }
}
