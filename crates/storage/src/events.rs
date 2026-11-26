//! Tenant event stream persistence (F017): the durable log behind
//! webhooks/SSE/projections, the consumer inbox and projection
//! checkpoints.
//!
//! Three invariants live in SQL rather than application memory:
//!
//! * **Gapless sequencing** — append locks the tenant row and stamps
//!   `MAX(seq)+1` inside the transaction, so the log itself never
//!   grows a hole, even with concurrent publishers. Consumers may
//!   still *observe* gaps through a lossy transport; the log is the
//!   replay source of truth.
//! * **Consumer dedup** — a claim is `INSERT ... ON CONFLICT DO
//!   NOTHING` on `(tenant, consumer, event_id)`: exactly one of N
//!   redeliveries applies the event. `release` hands the row back if
//!   the apply failed, so at-least-once delivery retries cleanly.
//! * **Checkpoints** — one cursor row per `(tenant, consumer)`;
//!   "everything ≤ last_seq is applied" is the baseline gap detection
//!   compares a delivered batch against.
//!
//! Every surface is tenant-bound via [`Store::begin_tenant`], with
//! row-level security as a second isolation layer.

use fishpond_domain::{AccountId, EventEnvelope, EventId, EventPayload, TenantId, UnixMillis};
use sqlx::postgres::Postgres;
use sqlx::{Row, Transaction};

use crate::error::{StorageError, StorageResult};
use crate::store::Store;

/// An event to append; the payload must be a JSON object (validated by
/// [`EventPayload::parse`] at construction).
pub struct NewEvent<'a> {
    /// Event id (unique per tenant).
    pub id: &'a EventId,
    /// Event type name (e.g. `message.sent`, `stream.gap`).
    pub event_type: &'a str,
    /// The account the event relates to, when applicable.
    pub account_id: Option<&'a AccountId>,
    /// When the event happened.
    pub occurred_at: UnixMillis,
    /// JSON object payload.
    pub payload: &'a EventPayload,
}

/// The result of an inbox claim.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InboxDecision {
    /// This consumer had not applied the event; it may apply it now.
    Claimed,
    /// Another delivery of the same event already applied it.
    Duplicate,
}

/// The inbox claim for one delivered event: exactly one of N
/// redeliveries proceeds ([`InboxDecision::Claimed`]), the rest see
/// [`InboxDecision::Duplicate`].
pub struct InboxClaim<'a> {
    /// The consuming projection/subscriber name.
    pub consumer: &'a str,
    /// The delivered event.
    pub event_id: &'a EventId,
    /// The event's sequence number (recorded for audits).
    pub seq: u64,
    /// Claim instant.
    pub at: UnixMillis,
}

impl Store {
    /// Appends events to the tenant's log, assigning the per-tenant
    /// sequence transactionally (tenant row locked → `MAX(seq)+1`),
    /// and returns the sealed envelopes with their sequences. The
    /// log's sequence has no gaps: concurrent publishers serialize on
    /// the tenant row. An empty batch appends nothing.
    pub async fn append_events(
        &self,
        tenant: &TenantId,
        events: &[NewEvent<'_>],
    ) -> StorageResult<Vec<EventEnvelope>> {
        let mut tx = self.begin_tenant(tenant).await?;
        let envelopes = append_events_tx(&mut tx, tenant, events).await?;
        tx.commit().await?;
        Ok(envelopes)
    }

    /// The log's highest sequence (0 when the tenant has no events).
    pub async fn latest_seq(&self, tenant: &TenantId) -> StorageResult<u64> {
        let mut tx = self.begin_tenant(tenant).await?;
        let latest: Option<i64> =
            sqlx::query_scalar("SELECT MAX(seq) FROM events WHERE tenant_id = $1")
                .bind(tenant.as_str())
                .fetch_one(&mut *tx)
                .await?;
        tx.commit().await?;
        Ok(latest.map_or(0, |v| u64::try_from(v).unwrap_or(0)))
    }

    /// Reads events strictly after `after_seq`, in sequence order,
    /// up to `limit` (0 means no limit). The catch-up/replay surface
    /// for projections and delivery transports.
    pub async fn list_events(
        &self,
        tenant: &TenantId,
        after_seq: u64,
        limit: u32,
    ) -> StorageResult<Vec<EventEnvelope>> {
        let mut tx = self.begin_tenant(tenant).await?;
        let after = i64::try_from(after_seq).map_err(|_| StorageError::Invalid {
            what: "after_seq",
            reason: format!("{after_seq} exceeds the sequence range"),
        })?;
        let rows = if limit == 0 {
            sqlx::query(
                "SELECT id, tenant_id, seq, event_type, account_id, occurred_at, payload
                 FROM events WHERE seq > $2 ORDER BY seq",
            )
            .bind(tenant.as_str())
            .bind(after)
            .fetch_all(&mut *tx)
            .await?
        } else {
            sqlx::query(
                "SELECT id, tenant_id, seq, event_type, account_id, occurred_at, payload
                 FROM events WHERE seq > $2 ORDER BY seq LIMIT $3",
            )
            .bind(tenant.as_str())
            .bind(after)
            .bind(i64::from(limit))
            .fetch_all(&mut *tx)
            .await?
        };
        tx.commit().await?;
        rows.iter().map(map_event).collect()
    }

    /// Fetches one event by id.
    pub async fn get_event(&self, tenant: &TenantId, id: &EventId) -> StorageResult<EventEnvelope> {
        let mut tx = self.begin_tenant(tenant).await?;
        let row = sqlx::query(
            "SELECT id, tenant_id, seq, event_type, account_id, occurred_at, payload
             FROM events WHERE id = $2",
        )
        .bind(tenant.as_str())
        .bind(id.as_str())
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(|| StorageError::NotFound {
            what: "event",
            id: id.as_str().to_owned(),
        })?;
        tx.commit().await?;
        map_event(&row)
    }

    /// Claims one delivered event for `consumer`: exactly one of N
    /// redeliveries gets [`InboxDecision::Claimed`]; the rest get
    /// [`InboxDecision::Duplicate`]. A claimed event that fails to
    /// apply should be released ([`Store::inbox_release`]) so the next
    /// delivery can retry.
    pub async fn inbox_claim(
        &self,
        tenant: &TenantId,
        claim: &InboxClaim<'_>,
    ) -> StorageResult<InboxDecision> {
        let mut tx = self.begin_tenant(tenant).await?;
        let seq = i64::try_from(claim.seq).map_err(|_| StorageError::Invalid {
            what: "event seq",
            reason: format!("{} exceeds the sequence range", claim.seq),
        })?;
        let inserted = sqlx::query(
            "INSERT INTO event_inbox (tenant_id, consumer, event_id, seq, claimed_at)
             VALUES ($1, $2, $3, $4, $5)
             ON CONFLICT (tenant_id, consumer, event_id) DO NOTHING",
        )
        .bind(tenant.as_str())
        .bind(claim.consumer)
        .bind(claim.event_id.as_str())
        .bind(seq)
        .bind(claim.at.as_millis())
        .execute(&mut *tx)
        .await?
        .rows_affected();
        tx.commit().await?;
        Ok(if inserted == 1 {
            InboxDecision::Claimed
        } else {
            InboxDecision::Duplicate
        })
    }

    /// Releases a claimed event whose apply failed, so the next
    /// delivery can retry it (at-least-once semantics).
    pub async fn inbox_release(
        &self,
        tenant: &TenantId,
        consumer: &str,
        event_id: &EventId,
    ) -> StorageResult<()> {
        let mut tx = self.begin_tenant(tenant).await?;
        sqlx::query(
            "DELETE FROM event_inbox
             WHERE consumer = $2 AND event_id = $3",
        )
        .bind(tenant.as_str())
        .bind(consumer)
        .bind(event_id.as_str())
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Advances the consumer's checkpoint to `last_seq` (the cursor
    /// never moves backwards: an older value is ignored).
    pub async fn advance_checkpoint(
        &self,
        tenant: &TenantId,
        consumer: &str,
        last_seq: u64,
        at: UnixMillis,
    ) -> StorageResult<()> {
        let mut tx = self.begin_tenant(tenant).await?;
        let seq = i64::try_from(last_seq).map_err(|_| StorageError::Invalid {
            what: "last_seq",
            reason: format!("{last_seq} exceeds the sequence range"),
        })?;
        sqlx::query(
            "INSERT INTO projection_checkpoints (tenant_id, consumer, last_seq, updated_at)
             VALUES ($1, $2, $3, $4)
             ON CONFLICT (tenant_id, consumer) DO UPDATE
                 SET last_seq = GREATEST(projection_checkpoints.last_seq, EXCLUDED.last_seq),
                     updated_at = EXCLUDED.updated_at",
        )
        .bind(tenant.as_str())
        .bind(consumer)
        .bind(seq)
        .bind(at.as_millis())
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// The consumer's checkpoint (0 when it never consumed).
    pub async fn checkpoint(&self, tenant: &TenantId, consumer: &str) -> StorageResult<u64> {
        let mut tx = self.begin_tenant(tenant).await?;
        let last: Option<i64> =
            sqlx::query_scalar("SELECT last_seq FROM projection_checkpoints WHERE consumer = $2")
                .bind(tenant.as_str())
                .bind(consumer)
                .fetch_optional(&mut *tx)
                .await?;
        tx.commit().await?;
        Ok(last.map_or(0, |v| u64::try_from(v).unwrap_or(0)))
    }
}

/// Appends events inside an existing tenant-bound transaction — the
/// composition seam for an outbox write: a state mutation and its
/// events commit atomically when both run in this transaction.
pub async fn append_events_tx(
    tx: &mut Transaction<'_, Postgres>,
    tenant: &TenantId,
    events: &[NewEvent<'_>],
) -> StorageResult<Vec<EventEnvelope>> {
    if events.is_empty() {
        return Ok(Vec::new());
    }
    // Serialize publishers of this tenant so MAX(seq)+1 cannot race a
    // hole into the sequence.
    sqlx::query("SELECT id FROM tenants WHERE id = $1 FOR UPDATE")
        .bind(tenant.as_str())
        .execute(&mut **tx)
        .await?;
    for event in events {
        if event.event_type.trim().is_empty() {
            return Err(StorageError::Invalid {
                what: "event type",
                reason: "must not be empty".to_owned(),
            });
        }
    }
    let mut envelopes = Vec::with_capacity(events.len());
    for event in events {
        let seq: i64 =
            sqlx::query_scalar("SELECT COALESCE(MAX(seq), 0) + 1 FROM events WHERE tenant_id = $1")
                .bind(tenant.as_str())
                .fetch_one(&mut **tx)
                .await?;
        let inserted = sqlx::query(
            "INSERT INTO events (id, tenant_id, seq, event_type, account_id, occurred_at, payload)
             VALUES ($1, $2, $3, $4, $5, $6, $7)
             ON CONFLICT (tenant_id, id) DO NOTHING",
        )
        .bind(event.id.as_str())
        .bind(tenant.as_str())
        .bind(seq)
        .bind(event.event_type)
        .bind(event.account_id.map(AccountId::as_str))
        .bind(event.occurred_at.as_millis())
        .bind(event.payload.as_str())
        .execute(&mut **tx)
        .await?
        .rows_affected();
        if inserted == 0 {
            // The same event id already exists: idempotent append —
            // return the stored row's envelope instead of a duplicate.
            let existing = sqlx::query(
                "SELECT id, tenant_id, seq, event_type, account_id, occurred_at, payload
                 FROM events WHERE id = $2",
            )
            .bind(tenant.as_str())
            .bind(event.id.as_str())
            .fetch_one(&mut **tx)
            .await?;
            envelopes.push(map_event(&existing)?);
            continue;
        }
        envelopes.push(EventEnvelope {
            id: event.id.clone(),
            tenant_id: tenant.clone(),
            seq: u64::try_from(seq).map_err(|_| StorageError::Invalid {
                what: "event seq",
                reason: format!("{seq} exceeds u64"),
            })?,
            event_type: event.event_type.to_owned(),
            account_id: event.account_id.cloned(),
            occurred_at: event.occurred_at,
            payload: event.payload.clone(),
        });
    }
    Ok(envelopes)
}

fn map_event(row: &sqlx::postgres::PgRow) -> StorageResult<EventEnvelope> {
    let raw_payload: String = row.try_get("payload")?;
    let account = row
        .try_get::<Option<String>, _>("account_id")?
        .map(AccountId::parse)
        .transpose()?;
    Ok(EventEnvelope {
        id: EventId::parse(row.try_get::<String, _>("id")?)?,
        tenant_id: TenantId::parse(row.try_get::<String, _>("tenant_id")?)?,
        seq: u64::try_from(row.try_get::<i64, _>("seq")?).map_err(|_| StorageError::Invalid {
            what: "event seq",
            reason: "stored sequence is negative".to_owned(),
        })?,
        event_type: row.try_get("event_type")?,
        account_id: account,
        occurred_at: UnixMillis::from_millis(row.try_get::<i64, _>("occurred_at")?)?,
        payload: EventPayload::parse(raw_payload)?,
    })
}
