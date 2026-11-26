//! Monitor errors: what the operator plane can hit.

use fishpond_storage::StorageError;

/// A monitor-layer failure.
#[derive(Debug)]
pub enum MonitorError {
    /// The storage layer failed underneath the monitor.
    Storage(StorageError),
    /// A required incident id was empty or blank.
    EmptyId,
}

impl std::fmt::Display for MonitorError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Storage(e) => write!(f, "monitor storage failure: {e}"),
            Self::EmptyId => write!(f, "incident id must not be empty"),
        }
    }
}

impl std::error::Error for MonitorError {}

impl From<StorageError> for MonitorError {
    fn from(e: StorageError) -> Self {
        Self::Storage(e)
    }
}

/// Monitor-layer result alias.
pub type MonitorResult<T> = Result<T, MonitorError>;
