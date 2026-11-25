//! The provider abstraction: one trait every engine implements.
//!
//! Calls carry a [`CallContext`] (tenant + account + idempotency key) so
//! implementations can enforce tenancy and deduplication without ambient
//! state. Write paths are idempotency-keyed; read paths are plain.

use async_trait::async_trait;

use fishpond_domain::{
    AccountId, Conversation, ConversationId, Message, Product, TenantId, UnixMillis,
};

use crate::manifest::ProviderManifest;
use crate::result::ProviderResult;
use crate::version::ProtocolVersion;

/// Identity and idempotency context of one provider call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallContext {
    /// Tenant whose data this call may touch.
    pub tenant: TenantId,
    /// Account the call acts on.
    pub account: AccountId,
    /// Caller-supplied idempotency key for write calls; read calls pass
    /// `None`.
    pub idempotency_key: Option<String>,
}

impl CallContext {
    /// Builds a write context (idempotency key required and validated).
    pub fn write(
        tenant: TenantId,
        account: AccountId,
        idempotency_key: impl Into<String>,
    ) -> ProviderResult<Self> {
        let key = idempotency_key.into();
        if key.trim().is_empty() {
            return Err(crate::result::ProviderError::new(
                crate::result::ErrorClass::Invalid,
                "empty_idempotency_key",
                "idempotency_key must not be empty on write calls",
            ));
        }
        Ok(Self {
            tenant,
            account,
            idempotency_key: Some(key),
        })
    }

    /// Builds a read context.
    pub fn read(tenant: TenantId, account: AccountId) -> Self {
        Self {
            tenant,
            account,
            idempotency_key: None,
        }
    }
}

/// Request to send one chat message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SendMessageRequest {
    /// Target conversation.
    pub conversation: ConversationId,
    /// Message body; providers must reject empty/whitespace-only text with
    /// [`crate::result::ErrorClass::Invalid`].
    pub text: String,
}

/// Result of establishing a session for an account.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthorizeOutcome {
    /// Opaque session reference (never a credential; providers keep the
    /// secret server-side or in the credential store).
    pub session_ref: String,
    /// When the session stops being usable.
    pub expires_at: UnixMillis,
}

/// Result of a send: the stored message plus dedup flag.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SendMessageOutcome {
    /// The message as recorded upstream.
    pub message: Message,
    /// True when the same idempotency key replayed an earlier send.
    pub deduplicated: bool,
}

/// The provider seam every engine implements (official API, browser, mock).
#[async_trait]
pub trait Provider: Send + Sync {
    /// Static manifest: capabilities, versions, engine.
    fn manifest(&self) -> &ProviderManifest;

    /// Agrees on the wire protocol with this provider.
    ///
    /// Default: negotiate over the manifest's declared versions.
    fn negotiate_protocol(&self, offer: &[ProtocolVersion]) -> ProviderResult<ProtocolVersion> {
        crate::version::negotiate(offer, &self.manifest().protocol_versions).map_err(|e| {
            crate::result::ProviderError::new(
                crate::result::ErrorClass::Unsupported,
                "protocol_negotiation_failed",
                e.to_string(),
            )
        })
    }

    /// Establishes a session for the account (idempotent per account).
    async fn authorize(&self, ctx: &CallContext) -> ProviderResult<AuthorizeOutcome>;

    /// Sends one message; deduplicates on the context's idempotency key.
    async fn send_message(
        &self,
        ctx: &CallContext,
        request: SendMessageRequest,
    ) -> ProviderResult<SendMessageOutcome>;

    /// Lists conversations of the context's account.
    async fn list_conversations(&self, ctx: &CallContext) -> ProviderResult<Vec<Conversation>>;

    /// Lists messages of one conversation, oldest first.
    async fn list_messages(
        &self,
        ctx: &CallContext,
        conversation: &ConversationId,
    ) -> ProviderResult<Vec<Message>>;

    /// Lists the tenant's product catalog.
    async fn list_products(&self, ctx: &CallContext) -> ProviderResult<Vec<Product>>;
}
