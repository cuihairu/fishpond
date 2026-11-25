//! The simulated upstream platform: world state, event log and fault
//! switches (the only CI target, §7.3).
//!
//! Everything is in-memory and deterministic. [`MockPlatform`] owns the
//! simulated world; [`crate::MockProvider`] adapts it to the
//! `provider_sdk::Provider` seam. Fault switches inject the failures the
//! scheduler/retry logic must survive: authorization denial, write
//! rejection, connection loss and duplicate event delivery.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use fishpond_domain::{
    Account, AccountId, Conversation, ConversationId, EventEnvelope, EventId, EventPayload,
    Message, MessageDirection, Product, ProductId, TenantId, UnixMillis,
};
use fishpond_provider_sdk::{
    AuthorizeOutcome, CallContext, ErrorClass, ProviderError, ProviderResult, SendMessageOutcome,
    SendMessageRequest,
};

/// Tenant served by [`MockPlatform::seeded`].
pub const SEED_TENANT: &str = "t-default";
/// Account served by [`MockPlatform::seeded`].
pub const SEED_ACCOUNT: &str = "acc-mock";
/// Conversation served by [`MockPlatform::seeded`].
pub const SEED_CONVERSATION: &str = "conv-mock-1";

/// Session lifetime granted by the mock authorize call, in milliseconds.
const SESSION_TTL_MS: i64 = 3_600_000;

/// Injectable failure modes; each is an independent on/off switch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MockFault {
    /// `authorize` fails with a retryable [`ErrorClass::Auth`] error.
    AuthorizationDenied,
    /// `send_message` fails with a retryable [`ErrorClass::Transient`]
    /// error; reads keep working.
    WriteRejection,
    /// Every call fails with [`ErrorClass::Timeout`] "connection_lost"
    /// until cleared; in-memory state (including idempotency) survives.
    Disconnect,
    /// The next emitted event is delivered twice (identical envelope) to
    /// exercise downstream deduplication; one-shot, clears itself.
    DuplicateNextEvent,
}

#[derive(Default)]
struct Flags {
    authorization_denied: bool,
    write_rejection: bool,
    disconnect: bool,
    duplicate_next_event: bool,
}

#[derive(Default)]
struct Inner {
    flags: Flags,
    accounts: HashMap<AccountId, Account>,
    conversations: HashMap<ConversationId, Conversation>,
    /// Messages per conversation id, oldest first.
    messages: HashMap<ConversationId, Vec<Message>>,
    products: Vec<Product>,
    /// account -> session expiry.
    sessions: HashMap<AccountId, UnixMillis>,
    /// (tenant, account, idempotency key) -> stored message id.
    sent: HashMap<(TenantId, AccountId, String), fishpond_domain::MessageId>,
    events: Vec<EventEnvelope>,
    /// Event sequence per tenant (event envelopes carry the tenant's seq).
    tenant_seq: HashMap<TenantId, u64>,
    next_event_id: u64,
    next_message_id: u64,
    now_ms: i64,
}

/// The simulated upstream platform.
///
/// Cheap to clone: state lives behind an inner mutex, so tests can keep a
/// handle for fault switches and event inspection while a
/// [`crate::MockProvider`] drives it.
#[derive(Clone, Default)]
pub struct MockPlatform {
    inner: Arc<Mutex<Inner>>,
}

/// Builds a [`ProviderError`] with the mock's stable codes.
fn mock_error(class: ErrorClass, code: &'static str, message: &'static str) -> ProviderError {
    ProviderError::new(class, code, message)
}

/// Fails the call while the disconnect fault is engaged.
fn guard_disconnect(inner: &Inner) -> ProviderResult<()> {
    if inner.flags.disconnect {
        return Err(mock_error(
            ErrorClass::Timeout,
            "connection_lost",
            "mock fault: connection to the platform is down",
        ));
    }
    Ok(())
}

/// Fails the call unless the account holds an unexpired session.
fn ensure_session(inner: &Inner, ctx: &CallContext) -> ProviderResult<()> {
    match inner.sessions.get(&ctx.account) {
        Some(expires_at) if expires_at.as_millis() > inner.now_ms => Ok(()),
        _ => Err(mock_error(
            ErrorClass::Auth,
            "not_authorized",
            "no live session for this account; call authorize first",
        )),
    }
}

/// Current mock clock as a timestamp.
fn ts(millis: i64) -> UnixMillis {
    UnixMillis::from_millis(millis).expect("mock clock is non-negative")
}

/// Appends one event; under [`MockFault::DuplicateNextEvent`] the same
/// envelope is delivered twice and the switch clears itself.
fn emit_event(
    inner: &mut Inner,
    tenant: &TenantId,
    account: Option<&AccountId>,
    event_type: &str,
    payload: serde_json::Value,
) {
    inner.next_event_id += 1;
    let seq = inner.tenant_seq.entry(tenant.clone()).or_insert(0);
    *seq += 1;
    let event_seq = *seq;
    let envelope = EventEnvelope {
        id: EventId::parse(format!("evt-{}", inner.next_event_id)).expect("mock event id is valid"),
        tenant_id: tenant.clone(),
        seq: event_seq,
        event_type: event_type.to_owned(),
        account_id: account.cloned(),
        occurred_at: ts(inner.now_ms),
        payload: EventPayload::parse(payload.to_string()).expect("json payload is an object"),
    };
    let duplicated = inner.flags.duplicate_next_event;
    if duplicated {
        inner.flags.duplicate_next_event = false;
    }
    inner.events.push(envelope.clone());
    if duplicated {
        inner.events.push(envelope);
    }
}

impl MockPlatform {
    /// An empty platform (no accounts, no data, no faults).
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner {
                now_ms: 1_000_000,
                ..Inner::default()
            })),
        }
    }

    /// A platform preloaded with the contract-suite fixture: tenant
    /// [`SEED_TENANT`], online account [`SEED_ACCOUNT`], conversation
    /// [`SEED_CONVERSATION`] with inbound history and one catalog item.
    pub fn seeded() -> Self {
        let platform = Self::new();
        let tenant = TenantId::parse(SEED_TENANT).expect("seed tenant id is valid");
        let account = AccountId::parse(SEED_ACCOUNT).expect("seed account id is valid");
        let conversation = ConversationId::parse(SEED_CONVERSATION).expect("seed conv id is valid");

        platform.add_account(
            Account::new(
                account.clone(),
                tenant.clone(),
                "mock",
                fishpond_domain::AccountState::Online,
                None,
                ts(1_000),
            )
            .expect("seed account is valid"),
        );
        platform.add_conversation(Conversation {
            id: conversation.clone(),
            account_id: account.clone(),
            tenant_id: tenant.clone(),
            updated_at: ts(2_000),
        });
        for (offset, text) in [(1_000, "hi there"), (1_500, "any update?")] {
            let message = Message::new(
                fishpond_domain::MessageId::parse(format!("msg-seed-{offset}"))
                    .expect("seed message id is valid"),
                conversation.clone(),
                MessageDirection::Inbound,
                text,
                ts(offset),
            )
            .expect("seed message is valid");
            platform.append_seed_message(conversation.clone(), message);
        }
        platform.seed_product(
            Product::new(
                ProductId::parse("prod-mock-1").expect("seed product id is valid"),
                tenant.clone(),
                "Fixture product",
                9_900,
                "CNY",
                fishpond_domain::ProductStatus::Listed,
                ts(1_000),
            )
            .expect("seed product is valid"),
        );
        platform
    }

    /// Turns a fault switch on or off.
    pub fn set_fault(&self, fault: MockFault, on: bool) {
        let mut inner = self.inner.lock().expect("mock platform lock");
        match fault {
            MockFault::AuthorizationDenied => inner.flags.authorization_denied = on,
            MockFault::WriteRejection => inner.flags.write_rejection = on,
            MockFault::Disconnect => inner.flags.disconnect = on,
            MockFault::DuplicateNextEvent => inner.flags.duplicate_next_event = on,
        }
    }

    /// Whether a fault switch is currently engaged.
    pub fn fault(&self, fault: MockFault) -> bool {
        let inner = self.inner.lock().expect("mock platform lock");
        match fault {
            MockFault::AuthorizationDenied => inner.flags.authorization_denied,
            MockFault::WriteRejection => inner.flags.write_rejection,
            MockFault::Disconnect => inner.flags.disconnect,
            MockFault::DuplicateNextEvent => inner.flags.duplicate_next_event,
        }
    }

    /// Registers an account in the simulated world.
    pub fn add_account(&self, account: Account) {
        let mut inner = self.inner.lock().expect("mock platform lock");
        inner.accounts.insert(account.id.clone(), account);
    }

    /// Registers a conversation in the simulated world.
    pub fn add_conversation(&self, conversation: Conversation) {
        let mut inner = self.inner.lock().expect("mock platform lock");
        inner
            .conversations
            .insert(conversation.id.clone(), conversation);
    }

    /// Adds a catalog item.
    pub fn seed_product(&self, product: Product) {
        let mut inner = self.inner.lock().expect("mock platform lock");
        inner.products.push(product);
    }

    /// Drains the event log (delivery order, duplicates included).
    pub fn take_events(&self) -> Vec<EventEnvelope> {
        let mut inner = self.inner.lock().expect("mock platform lock");
        std::mem::take(&mut inner.events)
    }

    /// Number of events currently buffered.
    pub fn event_count(&self) -> usize {
        self.inner.lock().expect("mock platform lock").events.len()
    }

    fn append_seed_message(&self, conversation: ConversationId, message: Message) {
        let mut inner = self.inner.lock().expect("mock platform lock");
        inner
            .messages
            .entry(conversation)
            .or_default()
            .push(message);
    }

    // ---- call surfaces used by the provider adapter ----

    pub(crate) fn authorize_call(&self, ctx: &CallContext) -> ProviderResult<AuthorizeOutcome> {
        let mut inner = self.inner.lock().expect("mock platform lock");
        guard_disconnect(&inner)?;
        if inner.flags.authorization_denied {
            return Err(mock_error(
                ErrorClass::Auth,
                "authorize_fault",
                "mock fault: authorization denied",
            ));
        }
        let expires_at = ts(inner.now_ms + SESSION_TTL_MS);
        inner.sessions.insert(ctx.account.clone(), expires_at);
        let who = ctx.account.as_str();
        Ok(AuthorizeOutcome {
            session_ref: format!("mock-session-{who}"),
            expires_at,
        })
    }

    pub(crate) fn send_call(
        &self,
        ctx: &CallContext,
        request: SendMessageRequest,
    ) -> ProviderResult<SendMessageOutcome> {
        let mut inner = self.inner.lock().expect("mock platform lock");
        guard_disconnect(&inner)?;
        ensure_session(&inner, ctx)?;
        if request.text.trim().is_empty() {
            return Err(mock_error(
                ErrorClass::Invalid,
                "empty_text",
                "text must not be empty",
            ));
        }
        let conversation = inner
            .conversations
            .get(&request.conversation)
            .ok_or_else(|| {
                mock_error(
                    ErrorClass::Invalid,
                    "conversation_not_found",
                    "no such conversation on this account",
                )
            })?;
        if conversation.tenant_id != ctx.tenant || conversation.account_id != ctx.account {
            return Err(mock_error(
                ErrorClass::Auth,
                "not_authorized",
                "conversation belongs to another tenant or account",
            ));
        }
        if inner.flags.write_rejection {
            return Err(mock_error(
                ErrorClass::Transient,
                "write_rejected",
                "mock fault: platform is rejecting writes",
            ));
        }

        let key = (
            ctx.tenant.clone(),
            ctx.account.clone(),
            ctx.idempotency_key.clone().unwrap_or_default(),
        );
        if let Some(id) = inner.sent.get(&key) {
            let messages = inner
                .messages
                .get(&request.conversation)
                .expect("sent message is stored under its conversation");
            let original = messages
                .iter()
                .find(|m| &m.id == id)
                .expect("idempotency map points at a stored message")
                .clone();
            return Ok(SendMessageOutcome {
                message: original,
                deduplicated: true,
            });
        }

        inner.next_message_id += 1;
        inner.now_ms += 1;
        let id = fishpond_domain::MessageId::parse(format!("msg-{}", inner.next_message_id))
            .expect("mock message id is valid");
        let message = Message::new(
            id.clone(),
            request.conversation.clone(),
            MessageDirection::Outbound,
            request.text.clone(),
            ts(inner.now_ms),
        )
        .expect("mock send text was validated non-empty");
        inner
            .messages
            .entry(request.conversation.clone())
            .or_default()
            .push(message.clone());
        inner.sent.insert(key, id.clone());
        let payload = serde_json::json!({
            "conversationId": request.conversation.as_str(),
            "messageId": id.as_str(),
            "text": request.text,
        });
        emit_event(
            &mut inner,
            &ctx.tenant,
            Some(&ctx.account),
            "message.sent",
            payload,
        );
        Ok(SendMessageOutcome {
            message,
            deduplicated: false,
        })
    }

    pub(crate) fn conversations_call(
        &self,
        ctx: &CallContext,
    ) -> ProviderResult<Vec<Conversation>> {
        let inner = self.inner.lock().expect("mock platform lock");
        guard_disconnect(&inner)?;
        ensure_session(&inner, ctx)?;
        Ok(inner
            .conversations
            .values()
            .filter(|c| c.tenant_id == ctx.tenant && c.account_id == ctx.account)
            .cloned()
            .collect())
    }

    pub(crate) fn messages_call(
        &self,
        ctx: &CallContext,
        conversation: &ConversationId,
    ) -> ProviderResult<Vec<Message>> {
        let inner = self.inner.lock().expect("mock platform lock");
        guard_disconnect(&inner)?;
        ensure_session(&inner, ctx)?;
        let record = inner.conversations.get(conversation).ok_or_else(|| {
            mock_error(
                ErrorClass::Invalid,
                "conversation_not_found",
                "no such conversation on this account",
            )
        })?;
        if record.tenant_id != ctx.tenant || record.account_id != ctx.account {
            return Err(mock_error(
                ErrorClass::Auth,
                "not_authorized",
                "conversation belongs to another tenant or account",
            ));
        }
        let mut messages = inner
            .messages
            .get(conversation)
            .cloned()
            .unwrap_or_default();
        messages.sort_by_key(|m| m.sent_at);
        Ok(messages)
    }

    pub(crate) fn products_call(&self, ctx: &CallContext) -> ProviderResult<Vec<Product>> {
        let inner = self.inner.lock().expect("mock platform lock");
        guard_disconnect(&inner)?;
        ensure_session(&inner, ctx)?;
        Ok(inner
            .products
            .iter()
            .filter(|p| p.tenant_id == ctx.tenant)
            .cloned()
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fault_switches_roundtrip() {
        let platform = MockPlatform::new();
        for fault in [
            MockFault::AuthorizationDenied,
            MockFault::WriteRejection,
            MockFault::Disconnect,
            MockFault::DuplicateNextEvent,
        ] {
            assert!(!platform.fault(fault), "{fault:?} starts off");
            platform.set_fault(fault, true);
            assert!(platform.fault(fault), "{fault:?} turns on");
            platform.set_fault(fault, false);
            assert!(!platform.fault(fault), "{fault:?} turns off");
        }
    }

    #[test]
    fn seeded_fixture_is_complete() {
        let platform = MockPlatform::seeded();
        let tenant = TenantId::parse(SEED_TENANT).unwrap();
        let account = AccountId::parse(SEED_ACCOUNT).unwrap();
        let conversation = ConversationId::parse(SEED_CONVERSATION).unwrap();
        let ctx = CallContext::read(tenant.clone(), account.clone());
        // Every call surface is session-gated, including the mock's.
        platform
            .authorize_call(&ctx)
            .expect("authorize the fixture account");

        let conversations = platform
            .conversations_call(&ctx)
            .expect("seeded reads work");
        assert_eq!(conversations.len(), 1);
        assert_eq!(conversations[0].id, conversation);

        let messages = platform
            .messages_call(&ctx, &conversation)
            .expect("seeded conversation is readable");
        assert_eq!(messages.len(), 2);
        assert!(messages.windows(2).all(|p| p[0].sent_at <= p[1].sent_at));

        let products = platform.products_call(&ctx).expect("catalog readable");
        assert_eq!(products.len(), 1);
        assert_eq!(products[0].id.as_str(), "prod-mock-1");

        assert_eq!(
            platform.take_events(),
            Vec::new(),
            "seeding emits no events"
        );
    }

    #[test]
    fn events_carry_per_tenant_sequence() {
        let platform = MockPlatform::new();
        let tenant = TenantId::parse(SEED_TENANT).unwrap();
        let other = TenantId::parse("t-other").unwrap();
        let account = AccountId::parse(SEED_ACCOUNT).unwrap();

        let mut inner = platform.inner.lock().unwrap();
        inner.now_ms += 1;
        emit_event(
            &mut inner,
            &tenant,
            Some(&account),
            "message.sent",
            serde_json::json!({"k": 1}),
        );
        emit_event(
            &mut inner,
            &other,
            None,
            "catalog.updated",
            serde_json::json!({"k": 2}),
        );
        emit_event(
            &mut inner,
            &tenant,
            Some(&account),
            "message.sent",
            serde_json::json!({"k": 3}),
        );
        drop(inner);

        let events = platform.take_events();
        assert_eq!(events.len(), 3);
        assert_eq!([events[0].seq, events[1].seq, events[2].seq], [1, 1, 2]);
        assert_eq!(events[0].id.as_str(), "evt-1");
        assert_eq!(events[1].id.as_str(), "evt-2");
        assert_eq!(events[2].id.as_str(), "evt-3");
        assert_eq!(events[0].payload.as_str(), r#"{"k":1}"#);
        assert_eq!(events[1].account_id, None);
    }
}
