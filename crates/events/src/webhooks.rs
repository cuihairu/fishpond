//! Webhook delivery service (F018): fan events out to subscribed
//! endpoints over an injected transport.
//!
//! The flow, end to end:
//!
//! 1. **Subscribe** — the endpoint passes the F018 SSRF guard
//!    ([`crate::ssrf::validate_endpoint`]) before it is stored; the
//!    signing secret is sealed under the F008 envelope.
//! 2. **Fan out** — [`WebhookService::enqueue`] snapshots each event
//!    into one queued delivery per matching subscription (enabled and,
//!    when set, matching the event type). Fan-out is idempotent per
//!    (subscription, event) pair.
//! 3. **Deliver** — [`WebhookService::deliver_due`] claims due rows,
//!    unseals the secret, signs the exact snapshotted body
//!    ([`crate::sign`]) and hands it to the transport. Transient
//!    failures re-park the row on an exponential schedule up to
//!    [`MAX_ATTEMPTS`]; permanent failures abandon it. Delivery is
//!    at-least-once: receivers dedup on the event id.
//! 4. **Replay** — [`WebhookService::replay`] (重投) revives any
//!    delivery with a fresh retry budget.
//!
//! The transport is injected ([`WebhookTransport`]): tests and CI use
//! an in-process mock, and the real HTTP client lands with the F032+
//! engine work — never before, per the authorization red line.

use async_trait::async_trait;
use fishpond_domain::{EventEnvelope, TenantId, UnixMillis};
use fishpond_storage::{
    CredentialCipher, DeliveryRecord, NewDelivery, NewWebhookSubscription, SecretBox, StorageError,
    StorageResult, Store, WebhookSubscriptionRecord,
};
use serde_json::json;

use crate::sign::{CONTENT_TYPE, SIGNATURE_HEADER, signature_header};
use crate::ssrf;

/// How many attempts a delivery gets before it is abandoned
/// (exponential backoff: 30 s, 1 m, 2 m, 4 m — the fifth failure
/// parks it for manual replay).
pub const MAX_ATTEMPTS: u32 = 5;

/// The webhook transport boundary. `headers` carries the content type
/// and the F018 signature; implementations must not log the signature
/// or the secret it was derived from.
#[async_trait]
pub trait WebhookTransport: Send + Sync {
    /// POSTs `body` to `url` with `headers`; the outcome drives the
    /// retry schedule. Implementations return outcome text that is
    /// safe to persist (status codes, reasons — never credentials).
    async fn deliver(
        &self,
        url: &str,
        headers: &[(&'static str, String)],
        body: &str,
    ) -> TransportOutcome;
}

/// What the transport made of an attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransportOutcome {
    /// The endpoint accepted the delivery (2xx).
    Success,
    /// A retryable failure (timeouts, 5xx, connection resets).
    Transient {
        /// Secret-free outcome text persisted with the delivery.
        reason: String,
    },
    /// A failure retrying will not fix (4xx other than 429).
    Permanent {
        /// Secret-free outcome text persisted with the delivery.
        reason: String,
    },
}

/// Counts from one [`WebhookService::deliver_due`] pass.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DeliveryReport {
    /// Deliveries the transport accepted.
    pub delivered: usize,
    /// Deliveries re-parked with a later `next_attempt_at`.
    pub retried: usize,
    /// Deliveries abandoned (permanent fault or budget exhausted).
    pub abandoned: usize,
}

/// The retry schedule: `30s * 2^(attempts_done - 1)`, capped at one
/// hour. `attempts_done` is the number of failed attempts so far.
pub fn retry_delay_ms(attempts_done: u32) -> u64 {
    const BASE_MS: u64 = 30_000;
    const CAP_MS: u64 = 3_600_000;
    let doubling = 1u64
        .checked_shl(attempts_done.saturating_sub(1))
        .unwrap_or(CAP_MS);
    BASE_MS.saturating_mul(doubling).min(CAP_MS)
}

/// The subscriber-facing webhook surface over one tenant-scoped store.
pub struct WebhookService {
    store: Store,
    cipher: CredentialCipher,
}

impl WebhookService {
    /// Wraps a store and the credential cipher whose master key seals
    /// subscription secrets.
    pub fn new(store: Store, cipher: CredentialCipher) -> Self {
        Self { store, cipher }
    }

    /// The underlying store (for composing around the same pool).
    pub fn store(&self) -> &Store {
        &self.store
    }

    /// Registers (or rotates) a subscription after the SSRF guard.
    /// The endpoint must be a public https URL; the signing secret is
    /// sealed before storage.
    pub async fn subscribe(
        &self,
        tenant: &TenantId,
        id: &str,
        endpoint: &str,
        secret: SecretBox,
        event_type: Option<&str>,
        at: UnixMillis,
    ) -> StorageResult<WebhookSubscriptionRecord> {
        ssrf::validate_endpoint(endpoint).map_err(|err| StorageError::Invalid {
            what: "webhook endpoint",
            reason: err.to_string(),
        })?;
        self.store
            .upsert_webhook_subscription(&NewWebhookSubscription {
                id,
                tenant,
                url: endpoint,
                cipher: &self.cipher,
                secret,
                event_type,
                at,
            })
            .await
    }

    /// Enables or disables a subscription.
    pub async fn set_enabled(
        &self,
        tenant: &TenantId,
        id: &str,
        enabled: bool,
        at: UnixMillis,
    ) -> StorageResult<()> {
        self.store
            .set_webhook_enabled(tenant, id, enabled, at)
            .await
    }

    /// Lists subscriptions (metadata only — no secret material).
    pub async fn list(&self, tenant: &TenantId) -> StorageResult<Vec<WebhookSubscriptionRecord>> {
        self.store.list_webhook_subscriptions(tenant).await
    }

    /// Queues one delivery per matching subscription for `event`.
    /// Returns the delivery ids (one per target; re-enqueuing the same
    /// event returns the same ids — fan-out is idempotent).
    pub async fn enqueue(
        &self,
        tenant: &TenantId,
        event: &EventEnvelope,
        at: UnixMillis,
    ) -> StorageResult<Vec<String>> {
        let subs = self.list(tenant).await?;
        let body = delivery_body(event).to_string();
        let targets: Vec<&WebhookSubscriptionRecord> = subs
            .iter()
            .filter(|sub| {
                sub.is_enabled
                    && sub
                        .event_type
                        .as_deref()
                        .is_none_or(|t| t == event.event_type)
            })
            .collect();
        let mut ids = Vec::with_capacity(targets.len());
        for sub in targets {
            let id = delivery_id(&sub.id, event.id.as_str());
            self.store
                .insert_delivery(&NewDelivery {
                    id: &id,
                    tenant,
                    subscription_id: &sub.id,
                    event_id: &event.id,
                    event_seq: event.seq,
                    body: &body,
                    at,
                })
                .await?;
            ids.push(id);
        }
        Ok(ids)
    }

    /// One delivery pass: claims due deliveries (and stale in-flight
    /// ones), signs each snapshotted body at `now`, hands it to
    /// `transport`, and settles the row by the outcome. `max` bounds
    /// the pass; `stale_after_ms` decides when an in-flight row counts
    /// as crashed and is reclaimed.
    pub async fn deliver_due(
        &self,
        tenant: &TenantId,
        transport: &dyn WebhookTransport,
        now: UnixMillis,
        stale_after_ms: i64,
        max: u32,
    ) -> StorageResult<DeliveryReport> {
        let claimed = self
            .store
            .claim_due_deliveries(tenant, now, stale_after_ms, max)
            .await?;
        let subscriptions = self.store.list_webhook_subscriptions(tenant).await?;
        let mut report = DeliveryReport::default();
        for row in claimed {
            let sub = match subscriptions
                .iter()
                .find(|sub| sub.id == row.subscription_id)
            {
                Some(sub) => sub,
                None => {
                    // The subscription was deleted mid-flight: nowhere
                    // to deliver to.
                    self.store
                        .settle_abandoned(tenant, &row.id, Some("subscription removed"), now)
                        .await?;
                    report.abandoned += 1;
                    continue;
                }
            };
            let secret = match self
                .store
                .open_webhook_secret(tenant, &sub.id, &self.cipher)
                .await
            {
                Ok(secret) => secret,
                Err(err) => return Err(err),
            };
            let headers = vec![
                ("content-type", CONTENT_TYPE.to_owned()),
                (
                    SIGNATURE_HEADER,
                    signature_header(secret.expose(), now.as_millis(), &row.body),
                ),
            ];
            match transport.deliver(&sub.url, &headers, &row.body).await {
                TransportOutcome::Success => {
                    self.store.settle_delivered(tenant, &row.id, now).await?;
                    report.delivered += 1;
                }
                TransportOutcome::Permanent { reason } => {
                    self.store
                        .settle_abandoned(tenant, &row.id, Some(&reason), now)
                        .await?;
                    report.abandoned += 1;
                }
                TransportOutcome::Transient { reason } => {
                    let attempts = row.attempts.saturating_add(1);
                    if attempts >= MAX_ATTEMPTS {
                        self.store
                            .settle_abandoned(tenant, &row.id, Some(&reason), now)
                            .await?;
                        report.abandoned += 1;
                    } else {
                        let next = UnixMillis::from_millis(
                            now.as_millis() + retry_delay_ms(attempts) as i64,
                        )
                        .map_err(|err| StorageError::Invalid {
                            what: "next attempt",
                            reason: err.to_string(),
                        })?;
                        self.store
                            .settle_retry(tenant, &row.id, attempts, next, Some(&reason), now)
                            .await?;
                        report.retried += 1;
                    }
                }
            }
        }
        Ok(report)
    }

    /// Manual redelivery (重投): resets a delivery — from any status,
    /// including delivered and abandoned — to pending with a fresh
    /// retry budget, due immediately.
    pub async fn replay(
        &self,
        tenant: &TenantId,
        delivery_id: &str,
        now: UnixMillis,
    ) -> StorageResult<()> {
        self.store.replay_delivery(tenant, delivery_id, now).await?;
        Ok(())
    }

    /// A delivery's current row (status, attempts, schedule, outcome).
    pub async fn delivery(
        &self,
        tenant: &TenantId,
        delivery_id: &str,
    ) -> StorageResult<DeliveryRecord> {
        self.store.get_delivery(tenant, delivery_id).await
    }
}

/// The exact JSON body a delivery POSTs. The payload is embedded as
/// parsed JSON (not a quoted string) so receivers see the original
/// object shape.
fn delivery_body(event: &EventEnvelope) -> serde_json::Value {
    let payload: serde_json::Value =
        serde_json::from_str(event.payload.as_str()).unwrap_or(json!({}));
    json!({
        "id": event.id.as_str(),
        "tenant": event.tenant_id.as_str(),
        "seq": event.seq,
        "type": event.event_type,
        "account": event.account_id.as_ref().map(|a| a.as_str()),
        "occurred_at": event.occurred_at.as_millis(),
        "payload": payload,
    })
}

/// Deterministic, collision-free per (subscription, event) delivery id
/// — re-enqueuing the same pair reuses the row instead of duplicating.
fn delivery_id(subscription_id: &str, event_id: &str) -> String {
    format!("whd-{subscription_id}-{event_id}")
}

#[cfg(test)]
mod tests {
    use super::{MAX_ATTEMPTS, retry_delay_ms};

    #[test]
    fn backoff_doubles_and_caps() {
        assert_eq!(retry_delay_ms(1), 30_000);
        assert_eq!(retry_delay_ms(2), 60_000);
        assert_eq!(retry_delay_ms(3), 120_000);
        assert_eq!(retry_delay_ms(4), 240_000);
        assert_eq!(retry_delay_ms(10), 3_600_000, "capped at one hour");
        assert_eq!(
            retry_delay_ms(0),
            30_000,
            "no attempts yet behaves like the first"
        );
        assert!(retry_delay_ms(u32::MAX) <= 3_600_000, "no overflow panic");
    }

    #[test]
    fn budget_is_five_attempts() {
        assert_eq!(MAX_ATTEMPTS, 5, "30s/1m/2m/4m backoff, then abandon");
    }

    #[test]
    fn delivery_ids_are_pair_deterministic() {
        use super::delivery_id;
        assert_eq!(
            delivery_id("wh-a", "evt-1"),
            delivery_id("wh-a", "evt-1"),
            "same pair, same id"
        );
        assert_ne!(delivery_id("wh-a", "evt-1"), delivery_id("wh-a", "evt-2"));
        assert_ne!(delivery_id("wh-a", "evt-1"), delivery_id("wh-b", "evt-1"));
    }
}
