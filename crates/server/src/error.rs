//! Service-layer errors: everything [`crate`] operations can fail with.

use fishpond_domain::DomainError;
use fishpond_storage::StorageError;

/// A service-layer failure.
#[derive(Debug)]
pub enum ServiceError {
    /// The requested resource does not exist or is not visible to the
    /// tenant (indistinguishable on purpose — no cross-tenant existence
    /// leaks).
    NotFound {
        /// Resource kind, e.g. `"account"`.
        what: &'static str,
        /// The requested id.
        id: String,
    },
    /// The caller submitted arguments that violate a domain invariant.
    Invalid {
        /// What was invalid, e.g. `"account id"`.
        what: &'static str,
        /// Why (never contains secrets).
        reason: String,
    },
    /// The operation would violate a state machine or a coordination rule
    /// (illegal transition, duplicate live session, ...).
    Conflict {
        /// What is in conflict, e.g. `"auth session"`.
        what: &'static str,
        /// The id of the conflicting resource.
        id: String,
        /// Why the operation conflicts with current state.
        reason: String,
    },
    /// Persistence failed.
    Storage(StorageError),
    /// A domain invariant was violated (bridged unchanged).
    Domain(DomainError),
}

impl ServiceError {
    /// Builds a [`ServiceError::NotFound`] with an owned id.
    pub fn not_found(what: &'static str, id: impl Into<String>) -> Self {
        Self::NotFound {
            what,
            id: id.into(),
        }
    }

    /// Builds a [`ServiceError::Invalid`].
    pub fn invalid(what: &'static str, reason: impl Into<String>) -> Self {
        Self::Invalid {
            what,
            reason: reason.into(),
        }
    }

    /// Builds a [`ServiceError::Conflict`].
    pub fn conflict(what: &'static str, id: impl Into<String>, reason: impl Into<String>) -> Self {
        Self::Conflict {
            what,
            id: id.into(),
            reason: reason.into(),
        }
    }
}

impl std::fmt::Display for ServiceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound { what, id } => write!(f, "{what} not found: {id}"),
            Self::Invalid { what, reason } => write!(f, "invalid {what}: {reason}"),
            Self::Conflict { what, id, reason } => {
                write!(f, "conflict on {what} {id}: {reason}")
            }
            Self::Storage(e) => write!(f, "storage error: {e}"),
            Self::Domain(e) => write!(f, "domain invariant violated: {e}"),
        }
    }
}

impl std::error::Error for ServiceError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Storage(e) => Some(e),
            Self::Domain(e) => Some(e),
            Self::NotFound { .. } | Self::Invalid { .. } | Self::Conflict { .. } => None,
        }
    }
}

impl From<StorageError> for ServiceError {
    fn from(e: StorageError) -> Self {
        match e {
            // Re-shape the common "row missing" case so callers see one
            // not-found shape regardless of layer.
            StorageError::NotFound { what, id } => Self::NotFound { what, id },
            other => Self::Storage(other),
        }
    }
}

impl From<DomainError> for ServiceError {
    fn from(e: DomainError) -> Self {
        Self::Domain(e)
    }
}

/// Result alias for service calls.
pub type ServiceResult<T> = Result<T, ServiceError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn storage_not_found_reshapes_not_wraps() {
        let err: ServiceError = StorageError::NotFound {
            what: "account",
            id: "acc-1".to_owned(),
        }
        .into();
        assert!(matches!(err, ServiceError::NotFound { what, .. } if what == "account"));
        assert!(err.to_string().contains("account not found"));
    }

    #[test]
    fn conflict_messages_name_the_resource() {
        let err =
            ServiceError::conflict("auth session", "s-1", "account already has a live session");
        assert!(err.to_string().contains("s-1"));
        assert!(err.to_string().contains("live session"));
    }
}
