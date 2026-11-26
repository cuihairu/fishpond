//! Queue-depth collection (F030 队列深度): one snapshot over the durable
//! queues — operations by state, attempts in flight and past deadline,
//! consumer lag — plus in-process mailbox samples pushed in by the
//! actor plane. A snapshot is the alert rules' input.

use fishpond_domain::TenantId;
use fishpond_runtime::MailboxSender;
use fishpond_storage::Store;

use crate::TelemetryResult;

/// One mailbox reading pushed in by the actor plane (the mailbox lives
/// in memory; telemetry never holds its handles).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MailboxDepthSample {
    /// Actor/mailbox name (e.g. "account-supervisor:acc-123").
    pub name: String,
    /// Queued messages right now.
    pub occupancy: usize,
    /// The mailbox's bound — occupancy plus the sender's free permits,
    /// exact under sequential use.
    pub capacity: usize,
}

impl MailboxDepthSample {
    /// Occupancy as a fraction of capacity (0.0 when unbounded-empty).
    pub fn ratio(&self) -> f64 {
        if self.capacity == 0 {
            return 0.0;
        }
        self.occupancy as f64 / self.capacity as f64
    }
}

/// Reads one mailbox's occupancy right now — the bridge the actor plane
/// calls per supervisor mailbox when taking a snapshot. Tokio's mpsc
/// `capacity()` counts *free permits*, not the bound, so the bound is
/// reconstructed as occupancy + free.
pub fn mailbox_sample<M>(name: impl Into<String>, sender: &MailboxSender<M>) -> MailboxDepthSample {
    let occupancy = sender.occupancy();
    let free = sender.capacity();
    MailboxDepthSample {
        name: name.into(),
        occupancy,
        capacity: occupancy + free,
    }
}

/// One tenant's durable-queue snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueueDepthReport {
    /// The tenant the snapshot was taken for.
    pub tenant: String,
    /// Per-state operation counts, ordered by state name.
    pub operations_by_state: Vec<(String, i64)>,
    /// Attempts with no outcome yet.
    pub in_flight_attempts: i64,
    /// In-flight attempts past their deadline (reaper backlog).
    pub overdue_attempts: i64,
    /// How far the named consumer trails the event head.
    pub consumer_lag: u64,
}

impl QueueDepthReport {
    /// The operation backlog: non-terminal operations only — terminal
    /// rows (`SUCCEEDED`/`FAILED`/`CANCELLED`) are kept for audit and
    /// must not count as outstanding work.
    pub fn backlog(&self) -> i64 {
        self.operations_by_state
            .iter()
            .filter(|(state, _)| !matches!(state.as_str(), "SUCCEEDED" | "FAILED" | "CANCELLED"))
            .map(|(_, n)| *n)
            .sum()
    }
}

/// Collects the queue-depth snapshot for `tenant` (its consumer named —
/// lag is per consumer).
pub async fn collect_queue_depth(
    store: &Store,
    tenant: &TenantId,
    consumer: &str,
) -> TelemetryResult<QueueDepthReport> {
    let report = QueueDepthReport {
        tenant: tenant.as_str().to_owned(),
        operations_by_state: store.operation_state_counts(tenant).await?,
        in_flight_attempts: store.in_flight_attempts(tenant).await?,
        overdue_attempts: store.overdue_attempts(tenant, now_ms()).await?,
        consumer_lag: store.consumer_lag(tenant, consumer).await?,
    };
    tracing::debug!(
        tenant = %report.tenant,
        backlog = report.backlog(),
        in_flight = report.in_flight_attempts,
        overdue = report.overdue_attempts,
        lag = report.consumer_lag,
        "queue depth collected"
    );
    Ok(report)
}

/// Wall-clock milliseconds — the depth queries' `now`.
fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use fishpond_runtime::MailboxPair;

    #[test]
    fn mailbox_samples_read_occupancy_and_capacity() {
        let (sender, _receiver) = MailboxPair::<u8>::bounded(4).split();
        sender.try_send(1).expect("room for one");
        sender.try_send(2).expect("room for two");

        let sample = mailbox_sample("supervisor:acc-1", &sender);
        assert_eq!(sample.capacity, 4);
        assert_eq!(sample.occupancy, 2);
        assert!((sample.ratio() - 0.5).abs() < 1e-9);
    }

    #[test]
    fn ratio_of_a_zero_capacity_mailbox_is_zero() {
        let sample = MailboxDepthSample {
            name: "x".into(),
            occupancy: 0,
            capacity: 0,
        };
        assert_eq!(sample.ratio(), 0.0);
    }

    #[test]
    fn backlog_counts_only_non_terminal_states() {
        let report = QueueDepthReport {
            tenant: "t".to_owned(),
            operations_by_state: vec![
                ("SUCCEEDED".to_owned(), 500),
                ("FAILED".to_owned(), 2),
                ("CANCELLED".to_owned(), 1),
                ("PENDING".to_owned(), 3),
                ("RUNNING".to_owned(), 1),
            ],
            in_flight_attempts: 0,
            overdue_attempts: 0,
            consumer_lag: 0,
        };
        assert_eq!(report.backlog(), 4);
    }
}
