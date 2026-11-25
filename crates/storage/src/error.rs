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
    /// Stored or submitted data violated a domain invariant.
    Domain(DomainError),
}

impl std::fmt::Display for StorageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Database(e) => write!(f, "database error: {e}"),
            Self::Migration(e) => write!(f, "migration error: {e}"),
            Self::NotFound { what, id } => write!(f, "{what} not found: {id}"),
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
            Self::NotFound { .. } => None,
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
