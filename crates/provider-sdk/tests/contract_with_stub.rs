//! F004 acceptance: an in-crate stub provider passes the contract suite,
//! and a deliberately broken implementation is caught by it.

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

use fishpond_domain::{
    AccountId, Conversation, ConversationId, Message, MessageDirection, Product, ProductId,
    ProductStatus, TenantId, UnixMillis,
};
use fishpond_provider_sdk::{
    CallContext, Capability, ContractFixture, EngineKind, ErrorClass, ProtocolVersion, Provider,
    ProviderError, ProviderManifest, ProviderResult, run_contract_suite,
};

const TENANT: &str = "t-default";
const ACCOUNT: &str = "acc-mock";
const CONVERSATION: &str = "conv-mock-1";

fn ts(millis: i64) -> UnixMillis {
    UnixMillis::from_millis(millis).unwrap()
}

/// In-memory provider stub. `corrupt_dedup` turns it into a deliberately
/// broken implementation the suite must catch.
struct StubProvider {
    manifest: ProviderManifest,
    state: Mutex<StubState>,
}

#[derive(Default)]
struct StubState {
    authorized: HashSet<AccountId>,
    conversations: HashMap<ConversationId, Conversation>,
    messages: HashMap<ConversationId, Vec<Message>>,
    /// idempotency key -> stored message id.
    sent: HashMap<String, fishpond_domain::MessageId>,
    next_ts: i64,
    corrupt_dedup: bool,
}

impl StubProvider {
    fn build(corrupt_dedup: bool) -> Self {
        let tenant = TenantId::parse(TENANT).unwrap();
        let account = AccountId::parse(ACCOUNT).unwrap();
        let conversation = ConversationId::parse(CONVERSATION).unwrap();

        let mut state = StubState {
            next_ts: 2_000,
            corrupt_dedup,
            ..StubState::default()
        };
        state.conversations.insert(
            conversation.clone(),
            Conversation {
                id: conversation.clone(),
                account_id: account.clone(),
                tenant_id: tenant.clone(),
                updated_at: ts(2_000),
            },
        );
        // Seed inbound history so the oldest-first check is meaningful.
        for (offset, text) in [(1_000, "hi there"), (1_500, "any update?")] {
            let message = Message::new(
                fishpond_domain::MessageId::parse(format!("msg-seed-{offset}")).unwrap(),
                conversation.clone(),
                MessageDirection::Inbound,
                text,
                ts(offset),
            )
            .unwrap();
            state
                .messages
                .entry(conversation.clone())
                .or_default()
                .push(message);
        }

        Self {
            manifest: ProviderManifest {
                provider_id: "stub".to_owned(),
                display_name: "Stub Provider".to_owned(),
                provider_version: "0.1.0".to_owned(),
                engine: EngineKind::Api,
                protocol_versions: vec![ProtocolVersion::V1_0, ProtocolVersion::V1_1],
                capabilities: vec![
                    Capability::AccountState,
                    Capability::Authorize,
                    Capability::ReadConversations,
                    Capability::SendMessage,
                    Capability::ReadCatalog,
                ],
            },
            state: Mutex::new(state),
        }
    }

    fn fixture() -> ContractFixture {
        ContractFixture {
            tenant: TenantId::parse(TENANT).unwrap(),
            account: AccountId::parse(ACCOUNT).unwrap(),
            conversation: ConversationId::parse(CONVERSATION).unwrap(),
        }
    }

    /// Auth-gate shared by every call surface.
    fn ensure_authorized(&self, ctx: &CallContext) -> ProviderResult<()> {
        let state = self.state.lock().unwrap();
        if state.authorized.contains(&ctx.account) {
            Ok(())
        } else {
            Err(ProviderError::new(
                ErrorClass::Auth,
                "not_authorized",
                "call authorize for this account first",
            ))
        }
    }

    fn next_message(
        state: &mut StubState,
        conversation: ConversationId,
        direction: MessageDirection,
        text: String,
    ) -> Message {
        let stamp = state.next_ts + 1;
        state.next_ts = stamp;
        let at = ts(stamp);
        Message::new(
            fishpond_domain::MessageId::parse(format!("msg-{stamp}")).unwrap(),
            conversation,
            direction,
            text,
            at,
        )
        .unwrap()
    }
}

#[async_trait::async_trait]
impl Provider for StubProvider {
    fn manifest(&self) -> &ProviderManifest {
        &self.manifest
    }

    async fn authorize(
        &self,
        ctx: &CallContext,
    ) -> ProviderResult<fishpond_provider_sdk::AuthorizeOutcome> {
        self.state
            .lock()
            .unwrap()
            .authorized
            .insert(ctx.account.clone());
        let who = ctx.account.as_str();
        Ok(fishpond_provider_sdk::AuthorizeOutcome {
            session_ref: format!("session-{who}"),
            expires_at: ts(1_000_000),
        })
    }

    async fn send_message(
        &self,
        ctx: &CallContext,
        request: fishpond_provider_sdk::SendMessageRequest,
    ) -> ProviderResult<fishpond_provider_sdk::SendMessageOutcome> {
        self.ensure_authorized(ctx)?;
        if request.text.trim().is_empty() {
            return Err(ProviderError::new(
                ErrorClass::Invalid,
                "empty_text",
                "text must not be empty",
            ));
        }
        let mut state = self.state.lock().unwrap();
        let conversation = state
            .conversations
            .get(&request.conversation)
            .ok_or_else(|| {
                ProviderError::new(
                    ErrorClass::Invalid,
                    "conversation_not_found",
                    "no such conversation on this account",
                )
            })?;
        if conversation.account_id != ctx.account || conversation.tenant_id != ctx.tenant {
            return Err(ProviderError::new(
                ErrorClass::Auth,
                "not_authorized",
                "conversation belongs to another account or tenant",
            ));
        }

        let key = ctx.idempotency_key.clone().unwrap_or_default();
        // An honest provider replays the message stored under the key; the
        // corrupt one invents a new one every call, which the suite catches.
        let replay = if state.corrupt_dedup {
            None
        } else {
            state.sent.get(&key).and_then(|id| {
                state
                    .messages
                    .get(&request.conversation)
                    .and_then(|messages| messages.iter().find(|m| &m.id == id).cloned())
            })
        };
        if let Some(message) = replay {
            return Ok(fishpond_provider_sdk::SendMessageOutcome {
                message,
                deduplicated: true,
            });
        }

        let message = Self::next_message(
            &mut state,
            request.conversation,
            MessageDirection::Outbound,
            request.text,
        );
        state.sent.insert(key, message.id.clone());
        state
            .messages
            .entry(message.conversation_id.clone())
            .or_default()
            .push(message.clone());
        Ok(fishpond_provider_sdk::SendMessageOutcome {
            message,
            deduplicated: false,
        })
    }

    async fn list_conversations(&self, ctx: &CallContext) -> ProviderResult<Vec<Conversation>> {
        self.ensure_authorized(ctx)?;
        let state = self.state.lock().unwrap();
        Ok(state
            .conversations
            .values()
            .filter(|c| c.account_id == ctx.account && c.tenant_id == ctx.tenant)
            .cloned()
            .collect())
    }

    async fn list_messages(
        &self,
        ctx: &CallContext,
        conversation: &ConversationId,
    ) -> ProviderResult<Vec<Message>> {
        self.ensure_authorized(ctx)?;
        let state = self.state.lock().unwrap();
        let mut messages = state
            .messages
            .get(conversation)
            .cloned()
            .unwrap_or_default();
        messages.sort_by_key(|m| m.sent_at);
        Ok(messages)
    }

    async fn list_products(&self, ctx: &CallContext) -> ProviderResult<Vec<Product>> {
        self.ensure_authorized(ctx)?;
        Ok(vec![
            Product::new(
                ProductId::parse("prod-1").unwrap(),
                ctx.tenant.clone(),
                "Fixture product",
                9_900,
                "CNY",
                ProductStatus::Listed,
                ts(1_000),
            )
            .unwrap(),
        ])
    }
}

#[tokio::test]
async fn honest_stub_passes_every_contract_check() {
    let provider = StubProvider::build(false);
    let report = run_contract_suite(&provider, &StubProvider::fixture()).await;
    report.assert_ok();
    assert_eq!(report.checks.len(), 9, "suite covers 9 invariants");
}

#[tokio::test]
async fn broken_dedup_is_caught_by_the_suite() {
    let provider = StubProvider::build(true);
    let report = run_contract_suite(&provider, &StubProvider::fixture()).await;
    assert_eq!(report.failures(), ["idempotent_replay"]);
}

#[tokio::test]
async fn negotiation_failure_maps_to_unsupported() {
    let provider = StubProvider::build(false);
    let err = provider
        .negotiate_protocol(&[ProtocolVersion::V2_0])
        .expect_err("v2-only offer must fail against a v1 provider");
    assert_eq!(err.class, ErrorClass::Unsupported);
    assert_eq!(err.code, "protocol_negotiation_failed");
    assert!(!err.retryable());
}

#[test]
fn write_context_rejects_blank_idempotency_key() {
    let err = CallContext::write(
        TenantId::parse(TENANT).unwrap(),
        AccountId::parse(ACCOUNT).unwrap(),
        "   ",
    )
    .expect_err("blank key must be rejected");
    assert_eq!(err.class, ErrorClass::Invalid);
    assert_eq!(err.code, "empty_idempotency_key");

    let ok = CallContext::read(
        TenantId::parse(TENANT).unwrap(),
        AccountId::parse(ACCOUNT).unwrap(),
    );
    assert_eq!(ok.idempotency_key, None);
}
