//! Core DTOs shared by every layer (std-only).
//!
//! These are the semantic shapes behind the REST/MCP wire DTOs. Wire layers
//! own their own serde structs and map from these; timestamps are
//! [`UnixMillis`] so the domain never depends on a date-time crate.

use crate::error::{DomainError, DomainResult};
use crate::id::{AccountId, ConversationId, EventId, MessageId, OperationId, ProductId, TenantId};
use crate::state::{AccountState, OperationState};

/// A UTC instant with millisecond precision (unix epoch millis).
///
/// Kept as a newtype so signatures cannot confuse timestamps with counts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct UnixMillis(i64);

impl UnixMillis {
    /// Wraps raw epoch milliseconds; rejects negative values.
    pub fn from_millis(millis: i64) -> DomainResult<Self> {
        if millis < 0 {
            return Err(DomainError::OutOfRange {
                field: "timestamp",
                constraint: "must be a non-negative epoch value",
            });
        }
        Ok(Self(millis))
    }

    /// Raw epoch milliseconds.
    pub fn as_millis(self) -> i64 {
        self.0
    }
}

/// Direction of a chat message relative to the account.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MessageDirection {
    /// Received from the counterparty.
    Inbound,
    /// Sent by the account.
    Outbound,
}

impl MessageDirection {
    /// Canonical wire name.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Inbound => "inbound",
            Self::Outbound => "outbound",
        }
    }
}

impl std::fmt::Display for MessageDirection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Catalog / lifecycle status of a product listing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ProductStatus {
    /// Listed and purchasable.
    Listed,
    /// Delisted but retained for history.
    Delisted,
    /// Removed from the platform.
    Removed,
}

impl ProductStatus {
    /// Canonical wire name.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Listed => "listed",
            Self::Delisted => "delisted",
            Self::Removed => "removed",
        }
    }
}

/// Outcome of one execution attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AttemptOutcome {
    /// Attempt completed the operation.
    Succeeded,
    /// Attempt failed terminally (not retried from this attempt).
    Failed,
    /// Attempt timed out; may be retried in a new attempt.
    TimedOut,
}

impl AttemptOutcome {
    /// Canonical wire name.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::TimedOut => "timed out",
        }
    }
}

/// One upstream platform account.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Account {
    /// Account id.
    pub id: AccountId,
    /// Owning tenant.
    pub tenant_id: TenantId,
    /// Platform identifier ("mock" in M0).
    pub platform: String,
    /// Current state-machine state.
    pub state: AccountState,
    /// Human-readable note, required when state is [`AccountState::Unknown`].
    pub state_note: Option<String>,
    /// Last update instant.
    pub updated_at: UnixMillis,
}

impl Account {
    /// Builds an account, enforcing the UNKNOWN-note invariant.
    pub fn new(
        id: AccountId,
        tenant_id: TenantId,
        platform: impl Into<String>,
        state: AccountState,
        state_note: Option<String>,
        updated_at: UnixMillis,
    ) -> DomainResult<Self> {
        let account = Self {
            id,
            tenant_id,
            platform: platform.into(),
            state,
            state_note,
            updated_at,
        };
        account.validate()?;
        Ok(account)
    }

    /// UNKNOWN accounts must carry a note; confirmed states must not carry
    /// a stale UNKNOWN note (it would contradict the confirmed state).
    pub fn validate(&self) -> DomainResult<()> {
        match self.state {
            AccountState::Unknown => {
                if self
                    .state_note
                    .as_deref()
                    .is_none_or(|note| note.trim().is_empty())
                {
                    return Err(DomainError::EmptyText {
                        field: "state_note (required while state is UNKNOWN)",
                    });
                }
            }
            _ => {
                if self.state_note.is_some() {
                    return Err(DomainError::OutOfRange {
                        field: "state_note",
                        constraint: "must be empty unless state is UNKNOWN",
                    });
                }
            }
        }
        Ok(())
    }
}

/// One chat thread on an account.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Conversation {
    /// Conversation id.
    pub id: ConversationId,
    /// Account the thread lives on.
    pub account_id: AccountId,
    /// Owning tenant.
    pub tenant_id: TenantId,
    /// Last activity instant.
    pub updated_at: UnixMillis,
}

/// One chat message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    /// Message id.
    pub id: MessageId,
    /// Thread the message belongs to.
    pub conversation_id: ConversationId,
    /// Inbound or outbound.
    pub direction: MessageDirection,
    /// Message body; must not be empty or whitespace-only.
    pub text: String,
    /// Send/receive instant.
    pub sent_at: UnixMillis,
}

impl Message {
    /// Builds a message, rejecting empty bodies.
    pub fn new(
        id: MessageId,
        conversation_id: ConversationId,
        direction: MessageDirection,
        text: impl Into<String>,
        sent_at: UnixMillis,
    ) -> DomainResult<Self> {
        let text = text.into();
        if text.trim().is_empty() {
            return Err(DomainError::EmptyText { field: "text" });
        }
        Ok(Self {
            id,
            conversation_id,
            direction,
            text,
            sent_at,
        })
    }
}

/// One catalog item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Product {
    /// Product id.
    pub id: ProductId,
    /// Owning tenant.
    pub tenant_id: TenantId,
    /// Display title.
    pub title: String,
    /// Price in the smallest currency unit (e.g. cents); must be >= 0.
    pub price_cents: i64,
    /// ISO-4217-ish currency code.
    pub currency: String,
    /// Listing status.
    pub status: ProductStatus,
    /// Last update instant.
    pub updated_at: UnixMillis,
}

impl Product {
    /// Builds a product, rejecting negative prices.
    pub fn new(
        id: ProductId,
        tenant_id: TenantId,
        title: impl Into<String>,
        price_cents: i64,
        currency: impl Into<String>,
        status: ProductStatus,
        updated_at: UnixMillis,
    ) -> DomainResult<Self> {
        if price_cents < 0 {
            return Err(DomainError::OutOfRange {
                field: "price_cents",
                constraint: "must be >= 0",
            });
        }
        Ok(Self {
            id,
            tenant_id,
            title: title.into(),
            price_cents,
            currency: currency.into(),
            status,
            updated_at,
        })
    }
}

/// One operation intent (what should happen) with its current state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Operation {
    /// Operation id.
    pub id: OperationId,
    /// Owning tenant.
    pub tenant_id: TenantId,
    /// What to do (e.g. "send_message").
    pub kind: String,
    /// Current state-machine state.
    pub state: OperationState,
    /// Creation instant.
    pub created_at: UnixMillis,
}

/// One execution attempt of an operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OperationAttempt {
    /// Attempt id.
    pub attempt_id: crate::id::AttemptId,
    /// Operation this attempt executes.
    pub operation_id: OperationId,
    /// 1-based attempt number.
    pub attempt_no: u32,
    /// Outcome of the attempt.
    pub outcome: AttemptOutcome,
    /// When the attempt started.
    pub started_at: UnixMillis,
}

impl OperationAttempt {
    /// Builds an attempt; attempt numbers start at 1.
    pub fn new(
        attempt_id: crate::id::AttemptId,
        operation_id: OperationId,
        attempt_no: u32,
        outcome: AttemptOutcome,
        started_at: UnixMillis,
    ) -> DomainResult<Self> {
        if attempt_no == 0 {
            return Err(DomainError::OutOfRange {
                field: "attempt_no",
                constraint: "must be >= 1",
            });
        }
        Ok(Self {
            attempt_id,
            operation_id,
            attempt_no,
            outcome,
            started_at,
        })
    }
}

/// Opaque JSON payload of an event (the domain does not parse it).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventPayload(String);

impl EventPayload {
    /// Wraps raw JSON text; must be a JSON object (starts with `{`).
    pub fn parse(raw: impl Into<String>) -> DomainResult<Self> {
        let raw = raw.into();
        if !raw.trim_start().starts_with('{') {
            return Err(DomainError::OutOfRange {
                field: "event payload",
                constraint: "must be a JSON object",
            });
        }
        Ok(Self(raw))
    }

    /// Raw JSON text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// One event in a tenant's stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventEnvelope {
    /// Event id.
    pub id: EventId,
    /// Owning tenant.
    pub tenant_id: TenantId,
    /// Tenant-scoped monotonic sequence number.
    pub seq: u64,
    /// Event type name (e.g. "message.sent").
    pub event_type: String,
    /// Account the event relates to, when applicable.
    pub account_id: Option<AccountId>,
    /// When the event happened.
    pub occurred_at: UnixMillis,
    /// JSON object payload.
    pub payload: EventPayload,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::id::{AccountId, AttemptId, MessageId};

    fn ts(millis: i64) -> UnixMillis {
        UnixMillis::from_millis(millis).unwrap()
    }

    #[test]
    fn timestamps_reject_negative_values() {
        assert!(UnixMillis::from_millis(-1).is_err());
        assert_eq!(UnixMillis::from_millis(0).unwrap().as_millis(), 0);
        assert_eq!(ts(1_000).as_millis(), 1_000);
    }

    #[test]
    fn unknown_account_requires_a_note() {
        let err = Account::new(
            AccountId::parse("acc-1").unwrap(),
            TenantId::parse("t-default").unwrap(),
            "mock",
            AccountState::Unknown,
            None,
            ts(100),
        )
        .expect_err("UNKNOWN without note must be rejected");
        assert_eq!(
            err.to_string(),
            "state_note (required while state is UNKNOWN) must not be empty"
        );
        let ok = Account::new(
            AccountId::parse("acc-1").unwrap(),
            TenantId::parse("t-default").unwrap(),
            "mock",
            AccountState::Unknown,
            Some("reconcile pending".to_owned()),
            ts(100),
        )
        .unwrap();
        assert_eq!(ok.state, AccountState::Unknown);
    }

    #[test]
    fn confirmed_states_must_not_carry_a_note() {
        assert!(
            Account::new(
                AccountId::parse("acc-1").unwrap(),
                TenantId::parse("t-default").unwrap(),
                "mock",
                AccountState::Online,
                Some("stale".to_owned()),
                ts(100),
            )
            .is_err()
        );
    }

    #[test]
    fn messages_reject_empty_bodies() {
        assert!(
            Message::new(
                MessageId::parse("m-1").unwrap(),
                ConversationId::parse("c-1").unwrap(),
                MessageDirection::Inbound,
                "   ",
                ts(1),
            )
            .is_err()
        );
        let ok = Message::new(
            MessageId::parse("m-1").unwrap(),
            ConversationId::parse("c-1").unwrap(),
            MessageDirection::Outbound,
            "可以，下午发出。",
            ts(1),
        )
        .unwrap();
        assert_eq!(ok.direction.as_str(), "outbound");
    }

    #[test]
    fn products_reject_negative_prices() {
        let build = |price| {
            Product::new(
                ProductId::parse("p-1").unwrap(),
                TenantId::parse("t-default").unwrap(),
                "Mechanical keyboard",
                price,
                "CNY",
                ProductStatus::Listed,
                ts(1),
            )
        };
        assert!(build(-1).is_err());
        assert!(build(0).is_ok());
        assert_eq!(build(29_900).unwrap().status.as_str(), "listed");
    }

    #[test]
    fn attempts_are_one_based() {
        assert!(
            OperationAttempt::new(
                AttemptId::parse("a-0").unwrap(),
                OperationId::parse("op-1").unwrap(),
                0,
                AttemptOutcome::Failed,
                ts(1),
            )
            .is_err()
        );
        assert!(
            OperationAttempt::new(
                AttemptId::parse("a-1").unwrap(),
                OperationId::parse("op-1").unwrap(),
                1,
                AttemptOutcome::Succeeded,
                ts(1),
            )
            .is_ok()
        );
    }

    #[test]
    fn event_payload_must_be_a_json_object() {
        assert!(EventPayload::parse("[1,2]").is_err());
        assert!(EventPayload::parse("").is_err());
        let ok = EventPayload::parse(r#"{"conversationId":"c-1"}"#).unwrap();
        assert!(ok.as_str().contains("conversationId"));
    }

    #[test]
    fn direction_display_is_the_wire_name() {
        assert_eq!(MessageDirection::Inbound.to_string(), "inbound");
        assert_eq!(AttemptOutcome::TimedOut.as_str(), "timed out");
    }
}
