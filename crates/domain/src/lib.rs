//! fishpond-domain: strong-typed ids, DTOs, error types and state machines
//! (§3.3 base layer; stays dependency-free, std-only).
//!
//! Wire layers (REST/MCP) map these types to their own serde structs; the
//! domain never gains dependencies for serialization or time formatting.

pub mod dto;
pub mod error;
pub mod id;
pub mod state;

pub use dto::{
    Account, AttemptOutcome, Conversation, EventEnvelope, EventPayload, Message, MessageDirection,
    Operation, OperationAttempt, Product, ProductStatus, UnixMillis,
};
pub use error::{DomainError, DomainResult, IdInvalidReason};
pub use id::{
    AccountId, ApiKeyId, AttemptId, AuthSessionId, ConversationId, CredentialId, EventId,
    MAX_ID_LEN, MessageId, OperationId, ProductId, TenantId,
};
pub use state::{AccountState, AuthSessionState, OperationState};

#[cfg(test)]
mod tests {
    use super::*;

    /// Cross-module smoke: build an account and move it through the machine.
    #[test]
    fn account_lifecycle_end_to_end() {
        let mut account = Account::new(
            AccountId::parse("acc-100").unwrap(),
            TenantId::parse("t-default").unwrap(),
            "mock",
            AccountState::Offline,
            None,
            UnixMillis::from_millis(1_000).unwrap(),
        )
        .unwrap();

        account.state = account.state.transition_to(AccountState::Online).unwrap();
        // observation lost during a reconcile
        account.state = account.state.transition_to(AccountState::Unknown).unwrap();
        assert_eq!(account.state.as_str(), "UNKNOWN");

        // reconcile confirms offline again
        account.state = account.state.transition_to(AccountState::Offline).unwrap();
        assert!(account.validate().is_ok());
    }

    /// Errors compose across modules and carry resource context.
    #[test]
    fn error_paths_are_contextual() {
        let err = OperationId::parse("op 1").unwrap_err();
        assert!(err.to_string().contains("operation"));
        let err = AccountState::parse("MAYBE").unwrap_err();
        assert!(err.to_string().contains("account state"));
    }
}
