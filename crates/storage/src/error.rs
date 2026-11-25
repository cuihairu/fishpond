//! Storage errors: everything that can go wrong below the domain.

use fishpond_domain::DomainError;

/// A storage-layer failure.
#[derive(Debug)]
pub enum StorageError {
    /// The database rejected or failed a query.
    Database(sqlx::Error),
    /// The schema migrations failed.
    Migration(sqlx::migrate::MigrateError),
    /// The requested row does not exist.
    NotFound {
        /// What kind of row (e.g. "tenant", "message").
        what: &'static str,
        /// The id that was not found.
        id: String,
    },
    /// An authenticated key lacks the scope required for an operation.
    PermissionDenied {
        /// The tenant of the key.
        tenant: String,
        /// The role carried by the key.
        role: String,
        /// The missing scope (e.g. "conversations:write").
        scope: String,
    },
    /// Submitted storage input is invalid before it reaches the database.
    Invalid {
        /// What kind of input (e.g. "api key").
        what: &'static str,
        /// Why.
        reason: String,
    },
    /// Stored or submitted data violated a domain invariant.
    Domain(DomainError),
}

impl std::fmt::Display for StorageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Database(e) => write!(f, "database error: {e}"),
            Self::Migration(e) => write!(f, "migration error: {e}"),
            Self::NotFound { what, id } => write!(f, "{what} not found: {id}"),
            Self::PermissionDenied {
                tenant,
                role,
                scope,
            } => write!(
                f,
                "permission denied: role {role} of tenant {tenant} lacks scope {scope}"
            ),
            Self::Invalid { what, reason } => write!(f, "invalid {what}: {reason}"),
            Self::Domain(e) => write!(f, "domain invariant violated: {e}"),
        }
    }
}

impl std::error::Error for StorageError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Database(e) => Some(e),
            Self::Migration(e) => Some(e),
            Self::Domain(e) => Some(e),
            // No underlying foreign error: these are storage's own verdicts.
            Self::NotFound { .. } | Self::PermissionDenied { .. } | Self::Invalid { .. } => None,
        }
    }
}

impl From<sqlx::Error> for StorageError {
    fn from(e: sqlx::Error) -> Self {
        Self::Database(e)
    }
}

impl From<sqlx::migrate::MigrateError> for StorageError {
    fn from(e: sqlx::migrate::MigrateError) -> Self {
        Self::Migration(e)
    }
}

impl From<DomainError> for StorageError {
    fn from(e: DomainError) -> Self {
        Self::Domain(e)
    }
}

/// Result alias for storage calls.
pub type StorageResult<T> = Result<T, StorageError>;
