//! Strong-typed resource ids (std-only newtypes over validated strings).

use std::fmt;

use crate::error::{DomainError, DomainResult, IdInvalidReason};

/// Maximum length of any resource id.
pub const MAX_ID_LEN: usize = 64;

macro_rules! define_id {
    ($(#[$doc:meta])* $name:ident => $what:literal) => {
        $(#[$doc])*
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name(String);

        impl $name {
            const WHAT: &'static str = $what;

            /// Validates and wraps a raw id string.
            pub fn parse(raw: impl Into<String>) -> DomainResult<Self> {
                let raw = raw.into();
                validate_id(&raw).map_err(|reason| DomainError::InvalidId {
                    what: Self::WHAT,
                    raw: raw.clone(),
                    reason,
                })?;
                Ok(Self(raw))
            }

            /// The validated id text.
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl TryFrom<String> for $name {
            type Error = DomainError;

            fn try_from(raw: String) -> DomainResult<Self> {
                Self::parse(raw)
            }
        }
    };
}

define_id!(
    /// Tenant whose data all resources belong to.
    TenantId => "tenant");
define_id!(
    /// One upstream platform account.
    AccountId => "account");
define_id!(
    /// One chat thread.
    ConversationId => "conversation");
define_id!(
    /// One chat message.
    MessageId => "message");
define_id!(
    /// One catalog item.
    ProductId => "product");
define_id!(
    /// One operation intent (dispatch unit).
    OperationId => "operation");
define_id!(
    /// One execution attempt of an operation.
    AttemptId => "attempt");
define_id!(
    /// One API key record.
    ApiKeyId => "api key");
define_id!(
    /// One stored credential.
    CredentialId => "credential");
define_id!(
    /// One stream event.
    EventId => "event");

/// Validates a raw id: non-empty, at most [`MAX_ID_LEN`], charset
/// `[A-Za-z0-9_-]` (safe in URLs, JSON keys and SQL literals).
fn validate_id(raw: &str) -> Result<(), IdInvalidReason> {
    if raw.is_empty() {
        return Err(IdInvalidReason::Empty);
    }
    if raw.len() > MAX_ID_LEN {
        return Err(IdInvalidReason::TooLong);
    }
    if !raw
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    {
        return Err(IdInvalidReason::BadCharset);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_well_formed_ids() {
        AccountId::parse("acc-100").expect("dash and digits are valid");
        TenantId::parse("t_default").expect("underscore is valid");
        assert_eq!(ProductId::parse("p1").expect("alnum").as_str(), "p1");
    }

    #[test]
    fn rejects_empty_too_long_and_bad_charset() {
        assert_eq!(
            AccountId::parse(""),
            Err(DomainError::InvalidId {
                what: "account",
                raw: String::new(),
                reason: IdInvalidReason::Empty,
            })
        );
        let long = "a".repeat(MAX_ID_LEN + 1);
        assert!(matches!(
            AccountId::parse(long),
            Err(DomainError::InvalidId {
                reason: IdInvalidReason::TooLong,
                ..
            })
        ));
        assert!(matches!(
            AccountId::parse("bad id!"),
            Err(DomainError::InvalidId {
                reason: IdInvalidReason::BadCharset,
                ..
            })
        ));
        // non-ascii is also rejected by the charset rule
        assert!(matches!(
            AccountId::parse("账号-1"),
            Err(DomainError::InvalidId {
                reason: IdInvalidReason::BadCharset,
                ..
            })
        ));
    }

    #[test]
    fn display_mentions_the_resource_kind_in_errors() {
        let err = CredentialId::parse("no spaces allowed").unwrap_err();
        assert!(err.to_string().contains("credential"));
        assert_eq!(AccountId::parse("acc-2").unwrap().to_string(), "acc-2");
    }

    #[test]
    fn ordering_is_lexicographic_and_stable() {
        let a = AccountId::parse("acc-2").unwrap();
        let b = AccountId::parse("acc-10").unwrap();
        // lexicographic, not numeric: deterministic ordering for storage keys
        assert!(b < a);
    }
}
