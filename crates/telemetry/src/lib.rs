//! fishpond-telemetry: the observability plane (F030) — tracing init,
//! an in-process metrics registry, queue-depth collection over the
//! durable queues, threshold-based resource alerts that open monitor
//! incidents, and the redaction scanner that keeps credentials out of
//! every surface above.
//!
//! Service seam (§3.3): builds on `fishpond-storage` and
//! `fishpond-runtime` (mailbox gauges) plus `fishpond-monitor` (alert
//! incidents); never on the delivery/front layers (`server`/`worker`/
//! `cli`), which call *into* this crate.
//!
//! The red line runs through everything here: a secret must not reach
//! logs, events, metrics or CLI output — [`redact`] is the last line of
//! defense and every rendered surface in this crate passes through it.

pub mod alerts;
pub mod depth;
pub mod metrics;
pub mod redact;
pub mod trace;

pub use alerts::{AlertThresholds, RaisedAlert, evaluate};
pub use depth::{MailboxDepthSample, QueueDepthReport, collect_queue_depth, mailbox_sample};
pub use metrics::{MetricKey, MetricKind, MetricPoint, MetricsRegistry};
pub use redact::{ScanConfig, ScanFinding, findings, sanitize};
pub use trace::init_tracing;

use fishpond_storage::StorageError;

/// A telemetry-layer failure.
#[derive(Debug)]
pub enum TelemetryError {
    /// The store failed underneath queue-depth collection.
    Storage(StorageError),
}

impl std::fmt::Display for TelemetryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Storage(e) => write!(f, "telemetry storage failure: {e}"),
        }
    }
}

impl std::error::Error for TelemetryError {}

impl From<StorageError> for TelemetryError {
    fn from(e: StorageError) -> Self {
        Self::Storage(e)
    }
}

/// Convenience alias for telemetry results.
pub type TelemetryResult<T> = Result<T, TelemetryError>;
