//! fishpond-events: the service side of the tenant event stream (F017).
//!
//! Storage (§3.3) owns the durable log, the consumer inbox and the
//! projection checkpoints; this crate wires them into the three flows
//! the platform needs:
//!
//! * **Publish (outbox write side)** — [`EventService::publish`] appends
//!   to the tenant log with a gapless per-tenant sequence. When the
//!   events must commit atomically with a state mutation, combine
//!   [`fishpond_storage::append_events_tx`] with the mutation's own
//!   transaction instead.
//! * **Consume (inbox)** — [`EventService::consume_batch`] applies a
//!   delivered batch at most once per consumer: redeliveries dedup on
//!   the inbox, a failed apply releases its claim so the retry works,
//!   and the checkpoint only ever certifies the contiguous prefix.
//! * **Gaps** — a lossy transport (webhooks, SSE) can drop or reorder
//!   deliveries even though the log itself never has a hole. A batch
//!   that does not pick up exactly at the consumer's checkpoint is
//!   reported with its missing sequences and applied *not at all* —
//!   order-dependent projections must not see event 3 before event 1.
//!   [`EventService::publish_gap`] seals the observation as a
//!   `stream.gap` event, and [`EventService::catch_up`] replays
//!   straight from the log, which is the authoritative repair path.
//!
//! Like the rest of the platform, every surface is tenant-bound and
//! deterministic: no clock reads, no ambient state.

mod sign;
mod ssrf;
mod webhooks;

pub use sign::{SIGNATURE_HEADER, VerifyError, signature_header, verify_header};
pub use ssrf::{EndpointError, validate_endpoint};
pub use webhooks::{
    DeliveryReport, MAX_ATTEMPTS, TransportOutcome, WebhookService, WebhookTransport,
    retry_delay_ms,
};

use fishpond_domain::{EventEnvelope, EventId, EventPayload, TenantId, UnixMillis};
use fishpond_storage::{InboxClaim, InboxDecision, NewEvent, StorageError, StorageResult, Store};

/// The synthetic event type that records an observed delivery gap.
pub const GAP_EVENT_TYPE: &str = "stream.gap";

/// What a consumed batch did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConsumeReport {
    /// Events this consumer applied now (claimed and passed to the
    /// apply closure successfully).
    pub applied: Vec<EventEnvelope>,
    /// Deliveries above the checkpoint that the inbox recognized as
    /// already applied. Events at or below the checkpoint are skipped
    /// as certified before the inbox is consulted, so they never
    /// appear here.
    pub duplicates: Vec<EventEnvelope>,
    /// Sequence numbers missing between the consumer's checkpoint and
    /// the delivered batch. Non-empty means **nothing** was applied:
    /// the batch must be replayed from the log after these arrive.
    pub missing_seqs: Vec<u64>,
    /// The consumer's checkpoint after this batch (the contiguous
    /// frontier — unchanged when `missing_seqs` is non-empty).
    pub checkpoint: u64,
}

/// Why a consume failed. The apply half carries the projection's own
/// error; the event's payload is never copied into this error.
#[derive(Debug)]
pub enum ConsumeError {
    /// The apply closure failed; its claim was released so the next
    /// delivery can retry.
    Apply {
        /// The event whose apply failed.
        event: EventId,
        /// The event's sequence number.
        seq: u64,
        /// The projection's own error.
        source: Box<dyn std::error::Error + Send + Sync>,
    },
    /// The store refused an operation.
    Storage(StorageError),
}

impl std::fmt::Display for ConsumeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConsumeError::Apply { event, seq, source } => {
                write!(f, "apply failed for event {event} (seq {seq}): {source}")
            }
            ConsumeError::Storage(err) => write!(f, "event store error: {err}"),
        }
    }
}

impl std::error::Error for ConsumeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ConsumeError::Apply { source, .. } => Some(source.as_ref()),
            ConsumeError::Storage(err) => Some(err),
        }
    }
}

impl From<StorageError> for ConsumeError {
    fn from(err: StorageError) -> Self {
        ConsumeError::Storage(err)
    }
}

/// Sequence numbers missing between `checkpoint` and a delivered
/// batch: everything from `checkpoint+1` up to the batch's highest
/// sequence that the batch does not carry. A batch that stops at the
/// checkpoint (or is empty) has no gap — "nothing delivered yet" is
/// not a hole.
fn gaps_after(checkpoint: u64, delivered: &[u64]) -> Vec<u64> {
    let future: Vec<u64> = delivered
        .iter()
        .copied()
        .filter(|seq| *seq > checkpoint)
        .collect();
    let Some(&highest) = future.iter().max() else {
        return Vec::new();
    };
    let delivered_set: std::collections::HashSet<u64> = future.into_iter().collect();
    (checkpoint + 1..=highest)
        .filter(|seq| !delivered_set.contains(seq))
        .collect()
}

/// Publishes and consumes the tenant event stream.
pub struct EventService {
    store: Store,
}

impl EventService {
    /// Wraps a connected store.
    pub fn new(store: Store) -> Self {
        Self { store }
    }

    /// The underlying store (for callers that compose other flows
    /// around the same connection pool).
    pub fn store(&self) -> &Store {
        &self.store
    }

    /// Appends events to the tenant log (the standalone publish path;
    /// for an atomic outbox write use [`fishpond_storage::append_events_tx`]
    /// inside the mutation's transaction). Returns the sealed envelopes
    /// with their assigned sequences.
    pub async fn publish(
        &self,
        tenant: &TenantId,
        events: &[NewEvent<'_>],
    ) -> StorageResult<Vec<EventEnvelope>> {
        self.store.append_events(tenant, events).await
    }

    /// Seals an observed delivery gap into the log as a `stream.gap`
    /// event, so operators and downstream consumers have a durable
    /// record that sequences went missing in transport. The gap event
    /// itself takes the next log sequence like any other event.
    pub async fn publish_gap(
        &self,
        tenant: &TenantId,
        id: &EventId,
        missing: &[u64],
        cause: &str,
        at: UnixMillis,
    ) -> StorageResult<EventEnvelope> {
        let payload = EventPayload::parse(
            serde_json::json!({ "missing": missing, "cause": cause }).to_string(),
        )
        .map_err(|err| StorageError::Invalid {
            what: "gap payload",
            reason: err.to_string(),
        })?;
        let mut sealed = self
            .store
            .append_events(
                tenant,
                &[NewEvent {
                    id,
                    event_type: GAP_EVENT_TYPE,
                    account_id: None,
                    occurred_at: at,
                    payload: &payload,
                }],
            )
            .await?;
        Ok(sealed.swap_remove(0))
    }

    /// Applies a delivered batch for one consumer, at most once per
    /// event:
    ///
    /// * Deliveries at or below the consumer's checkpoint are already
    ///   certified — skipped without touching the inbox.
    /// * If the batch does not pick up exactly at `checkpoint+1`, or
    ///   has a hole inside, **nothing is applied**: the report carries
    ///   [`ConsumeReport::missing_seqs`] and the caller replays from
    ///   the log. Projections must not observe event 3 before event 1.
    /// * Otherwise each event is claimed in the inbox (redeliveries of
    ///   an already-applied event come back as duplicates), passed to
    ///   `apply`, and the checkpoint advances to the batch's end. A
    ///   failed apply releases its claim and aborts the batch.
    pub async fn consume_batch<F>(
        &self,
        tenant: &TenantId,
        consumer: &str,
        batch: &[EventEnvelope],
        at: UnixMillis,
        mut apply: F,
    ) -> Result<ConsumeReport, ConsumeError>
    where
        F: FnMut(&EventEnvelope) -> Result<(), Box<dyn std::error::Error + Send + Sync>>,
    {
        let checkpoint = self.store.checkpoint(tenant, consumer).await?;

        // Oldest first; duplicates within one batch (the same event
        // delivered twice in transit) collapse to their first copy.
        let mut delivered: Vec<&EventEnvelope> =
            batch.iter().filter(|env| env.seq > checkpoint).collect();
        delivered.sort_by_key(|env| env.seq);
        delivered.dedup_by_key(|env| env.seq);

        let seqs: Vec<u64> = delivered.iter().map(|env| env.seq).collect();
        let missing = gaps_after(checkpoint, &seqs);
        if !missing.is_empty() {
            return Ok(ConsumeReport {
                applied: Vec::new(),
                duplicates: Vec::new(),
                missing_seqs: missing,
                checkpoint,
            });
        }

        let mut applied = Vec::with_capacity(delivered.len());
        let mut duplicates = Vec::new();
        for env in delivered {
            let claim = InboxClaim {
                consumer,
                event_id: &env.id,
                seq: env.seq,
                at,
            };
            let decision = self.store.inbox_claim(tenant, &claim).await?;
            if decision == InboxDecision::Duplicate {
                duplicates.push(env.clone());
                continue;
            }
            if let Err(source) = apply(env) {
                // Give the claim back so the retry can apply this
                // event; the batch aborts here (nothing after it).
                self.store.inbox_release(tenant, consumer, &env.id).await?;
                return Err(ConsumeError::Apply {
                    event: env.id.clone(),
                    seq: env.seq,
                    source,
                });
            }
            applied.push(env.clone());
        }

        // The batch was contiguous from checkpoint+1 (checked above),
        // so the frontier is simply its last sequence.
        let frontier = applied
            .iter()
            .chain(duplicates.iter())
            .map(|env| env.seq)
            .max()
            .unwrap_or(checkpoint);
        if frontier > checkpoint {
            self.store
                .advance_checkpoint(tenant, consumer, frontier, at)
                .await?;
        }
        Ok(ConsumeReport {
            applied,
            duplicates,
            missing_seqs: Vec::new(),
            checkpoint: frontier,
        })
    }

    /// Catches a consumer up by replaying the log from its checkpoint —
    /// the authoritative repair path after a reported gap. The log has
    /// no holes, so the report never carries missing sequences; `limit`
    /// caps how much is replayed per pass (0 means everything).
    pub async fn catch_up<F>(
        &self,
        tenant: &TenantId,
        consumer: &str,
        limit: u32,
        at: UnixMillis,
        apply: F,
    ) -> Result<ConsumeReport, ConsumeError>
    where
        F: FnMut(&EventEnvelope) -> Result<(), Box<dyn std::error::Error + Send + Sync>>,
    {
        let checkpoint = self.store.checkpoint(tenant, consumer).await?;
        let replay = self.store.list_events(tenant, checkpoint, limit).await?;
        self.consume_batch(tenant, consumer, &replay, at, apply)
            .await
    }

    /// The consumer's checkpoint (0 when it never consumed).
    pub async fn checkpoint(&self, tenant: &TenantId, consumer: &str) -> StorageResult<u64> {
        self.store.checkpoint(tenant, consumer).await
    }
}

#[cfg(test)]
mod tests {
    use super::gaps_after;

    #[test]
    fn no_gap_when_the_batch_picks_up_at_the_checkpoint() {
        assert!(
            gaps_after(0, &[]).is_empty(),
            "nothing delivered is not a hole"
        );
        assert!(gaps_after(0, &[1, 2, 3]).is_empty());
        assert!(gaps_after(2, &[3]).is_empty());
    }

    #[test]
    fn gap_is_everything_between_checkpoint_and_the_batch() {
        assert_eq!(gaps_after(0, &[3, 4]), vec![1, 2], "batch starts late");
        assert_eq!(gaps_after(2, &[4]), vec![3]);
        assert_eq!(gaps_after(2, &[3, 5]), vec![4], "hole inside the batch");
        assert_eq!(
            gaps_after(5, &[3, 4]),
            Vec::<u64>::new(),
            "stale deliveries are not a gap"
        );
    }
}
