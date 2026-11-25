//! Service seam between MCP tools and the Fishpond business layer.
//!
//! Tools in [`crate::tools`] call these traits only; they never talk to a
//! provider directly (§provider-sdk abstraction stays the only upstream
//! path). In M0 the sole implementation is [`crate::mock::MockServices`];
//! F025 wires the real server service layer here.

use crate::auth::AuthContext;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

/// Read-model note attached to accounts whose state cannot be verified.
pub const UNKNOWN_STATE_NOTE: &str = "UNKNOWN means the observed state could not be verified; \
treat the account as neither confirmed online nor offline until a reconcile succeeds";

/// One account under management, as exposed to agents.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Account {
    /// Stable account identifier.
    pub account_id: String,
    /// Owning tenant (equals the authenticated tenant; shown for clarity).
    pub tenant_id: String,
    /// Platform the account belongs to (`mock` in M0).
    pub platform: String,
    /// Account state machine string, e.g. `ONLINE`, `DEGRADED`, `UNKNOWN`.
    pub state: String,
    /// Human-readable qualifier for the current state, if any.
    pub state_note: Option<String>,
    /// RFC3339 timestamp of the last state observation.
    pub updated_at: String,
}

/// A conversation between a managed account and a counterpart.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Conversation {
    /// Stable conversation identifier.
    pub conversation_id: String,
    /// Managed account participating in the conversation.
    pub account_id: String,
    /// Counterpart identifier (buyer on the mock platform).
    pub peer_id: String,
    /// Short summary of the latest exchange.
    pub summary: String,
    /// RFC3339 timestamp of the last message.
    pub updated_at: String,
}

/// Direction of a message relative to the managed account.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub enum MessageDirection {
    /// Sent by the managed account.
    Outbound,
    /// Received by the managed account.
    Inbound,
}

/// One message inside a conversation.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Message {
    /// Stable message identifier.
    pub message_id: String,
    /// Conversation the message belongs to.
    pub conversation_id: String,
    /// Direction relative to the managed account.
    pub direction: MessageDirection,
    /// Message body.
    pub text: String,
    /// RFC3339 send timestamp.
    pub sent_at: String,
}

/// Result of an accepted (Mock-path) send.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SendMessageOutcome {
    /// The stored message (replayed original on duplicate keys).
    pub message: Message,
    /// `true` when the Idempotency-Key was already seen and no new message
    /// was created.
    pub deduplicated: bool,
}

/// A catalog product.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Product {
    /// Stable product identifier.
    pub product_id: String,
    /// Product title.
    pub title: String,
    /// Price in minor units (cents).
    pub price_cents: i64,
    /// Currency code, e.g. `CNY`.
    pub currency: String,
    /// Listing status.
    pub status: String,
    /// RFC3339 timestamp of the last catalog sync.
    pub updated_at: String,
}

/// Summary of one attempt inside an operation.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct OperationAttempt {
    /// Stable attempt identifier.
    pub attempt_id: String,
    /// RFC3339 start timestamp.
    pub started_at: String,
    /// Outcome label, e.g. `succeeded`, `failed`, `in_flight`.
    pub outcome: String,
}

/// An operation (intent) with its attempt history.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Operation {
    /// Stable operation identifier.
    pub operation_id: String,
    /// Operation kind, e.g. `send_message`.
    pub kind: String,
    /// Account the operation targets.
    pub account_id: String,
    /// Aggregate status, e.g. `succeeded`, `failed`, `in_progress`.
    pub status: String,
    /// RFC3339 creation timestamp.
    pub created_at: String,
    /// Attempt summaries, oldest first.
    pub attempts: Vec<OperationAttempt>,
}

/// One event from the account event stream.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct EventEnvelope {
    /// Stable event identifier.
    pub event_id: String,
    /// Per-tenant monotonic sequence number.
    pub seq: u64,
    /// Event type, e.g. `message.sent`, `account.state_changed`.
    pub event_type: String,
    /// Account the event refers to.
    pub account_id: String,
    /// RFC3339 occurrence timestamp.
    pub occurred_at: String,
    /// Type-specific payload.
    pub payload: serde_json::Value,
}

/// Cursor-paginated list result.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Page<T> {
    /// Items on this page.
    pub items: Vec<T>,
    /// Cursor to pass for the next page, `None` when exhausted.
    pub next_cursor: Option<String>,
}

/// Errors surfaced by the service seam, mapped 1:1 onto tool errors.
#[derive(Debug, Clone, PartialEq)]
pub enum ServiceError {
    /// No or unknown API key presented.
    Unauthenticated,
    /// The key lacks the scope required by the tool.
    PermissionDenied {
        /// Name of the missing scope.
        scope: &'static str,
    },
    /// Resource unknown or not in the caller's tenant (indistinguishable
    /// on purpose, to avoid cross-tenant existence leaks).
    NotFound {
        /// Resource label for the message, e.g. `"account"`.
        what: &'static str,
        /// Identifier that was requested.
        id: String,
    },
    /// Caller-supplied arguments are invalid.
    InvalidArgument {
        /// Explanation (never contains secrets).
        reason: String,
    },
}

impl std::fmt::Display for ServiceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ServiceError::Unauthenticated => {
                write!(f, "unauthenticated: present a valid API key")
            }
            ServiceError::PermissionDenied { scope } => {
                write!(f, "permission denied: missing scope {scope}")
            }
            ServiceError::NotFound { what, id } => write!(f, "{what} not found: {id}"),
            ServiceError::InvalidArgument { reason } => write!(f, "invalid argument: {reason}"),
        }
    }
}

impl std::error::Error for ServiceError {}

/// Accounts read path.
#[async_trait]
pub trait AccountService: Send + Sync {
    /// Lists accounts of the caller's tenant.
    async fn list(
        &self,
        ctx: &AuthContext,
        cursor: Option<String>,
        limit: usize,
    ) -> Result<Page<Account>, ServiceError>;

    /// Returns one account of the caller's tenant.
    async fn get(&self, ctx: &AuthContext, account_id: &str) -> Result<Account, ServiceError>;
}

/// Conversations, messages and the Mock send path.
#[async_trait]
pub trait MessagingService: Send + Sync {
    /// Lists conversations, optionally filtered by account.
    async fn list_conversations(
        &self,
        ctx: &AuthContext,
        account_id: Option<&str>,
        cursor: Option<String>,
        limit: usize,
    ) -> Result<Page<Conversation>, ServiceError>;

    /// Lists messages of one conversation.
    async fn list_messages(
        &self,
        ctx: &AuthContext,
        conversation_id: &str,
        cursor: Option<String>,
        limit: usize,
    ) -> Result<Page<Message>, ServiceError>;

    /// Sends a message through the provider (Mock in M0), deduplicated by
    /// `idempotency_key` per tenant.
    async fn send(
        &self,
        ctx: &AuthContext,
        conversation_id: &str,
        text: &str,
        idempotency_key: &str,
    ) -> Result<SendMessageOutcome, ServiceError>;
}

/// Product catalog read path.
#[async_trait]
pub trait CatalogService: Send + Sync {
    /// Lists products of the caller's tenant.
    async fn list(
        &self,
        ctx: &AuthContext,
        cursor: Option<String>,
        limit: usize,
    ) -> Result<Page<Product>, ServiceError>;

    /// Returns one product of the caller's tenant.
    async fn get(&self, ctx: &AuthContext, product_id: &str) -> Result<Product, ServiceError>;
}

/// Operations and attempts read path.
#[async_trait]
pub trait OperationService: Send + Sync {
    /// Lists operations of the caller's tenant.
    async fn list(
        &self,
        ctx: &AuthContext,
        cursor: Option<String>,
        limit: usize,
    ) -> Result<Page<Operation>, ServiceError>;

    /// Returns one operation (with attempt summaries) of the caller's tenant.
    async fn get(&self, ctx: &AuthContext, operation_id: &str) -> Result<Operation, ServiceError>;
}

/// Event stream read path (snapshot in M0; live push in M1/F017).
#[async_trait]
pub trait EventService: Send + Sync {
    /// Returns the most recent events of the caller's tenant.
    async fn recent(
        &self,
        ctx: &AuthContext,
        limit: usize,
    ) -> Result<Vec<EventEnvelope>, ServiceError>;
}
