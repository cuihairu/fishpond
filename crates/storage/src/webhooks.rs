//! Webhook subscription and delivery persistence (F018).
//!
//! `webhook_subscriptions` is the tenant endpoint registry; its signing
//! secret is stored under the F008 envelope (per-subscription DEK
//! wrapped by a master key) so only ciphertext reaches the database.
//! `webhook_deliveries` is the fan-out queue from the F017 event log:
//! each row snapshots the exact body it will POST, so retries sign the
//! same bytes. Delivery is at-least-once — `claim_due_deliveries`
//! flips due `pending` rows (and stale `delivering` rows left by a
//! crashed loop) to `delivering` atomically, and the caller settles
//! each row with the transport's outcome.
//!
//! Every surface is tenant-bound via [`Store::begin_tenant`] with row
//! level security as the second isolation layer.

use fishpond_domain::{EventId, TenantId, UnixMillis};
use sqlx::Row;

use crate::credentials::{CredentialCipher, SealedSecret, SecretBox};
use crate::error::{StorageError, StorageResult};
use crate::store::Store;

/// A delivery's position in its lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeliveryStatus {
    /// Waiting for `next_attempt_at` to come due.
    Pending,
    /// Claimed by a delivery loop and in flight.
    Delivering,
    /// The transport accepted it (at-least-once: it may still be
    /// replayed by hand).
    Delivered,
    /// Permanent failure or the retry budget ran out; only an explicit
    /// replay revives it.
    Abandoned,
}

impl DeliveryStatus {
    /// The database representation.
    pub fn as_str(&self) -> &'static str {
        match self {
            DeliveryStatus::Pending => "pending",
            DeliveryStatus::Delivering => "delivering",
            DeliveryStatus::Delivered => "delivered",
            DeliveryStatus::Abandoned => "abandoned",
        }
    }

    /// Parses the database representation.
    pub fn parse(raw: &str) -> StorageResult<Self> {
        match raw {
            "pending" => Ok(DeliveryStatus::Pending),
            "delivering" => Ok(DeliveryStatus::Delivering),
            "delivered" => Ok(DeliveryStatus::Delivered),
            "abandoned" => Ok(DeliveryStatus::Abandoned),
            other => Err(StorageError::Invalid {
                what: "delivery status",
                reason: format!("unknown status {other:?}"),
            }),
        }
    }
}

/// A subscription's metadata — everything but the secret material.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebhookSubscriptionRecord {
    /// Subscription id.
    pub id: String,
    /// Owning tenant.
    pub tenant_id: TenantId,
    /// Delivery endpoint (validated before it got here).
    pub url: String,
    /// Master-key generation the secret is wrapped under.
    pub kek_version: i32,
    /// Secret fingerprint (correlation only, never the secret).
    pub fingerprint: Option<String>,
    /// The event type this subscription receives (`None` = all).
    pub event_type: Option<String>,
    /// Whether deliveries fan out to it.
    pub is_enabled: bool,
    /// Creation instant.
    pub created_at: UnixMillis,
    /// Last mutation instant.
    pub updated_at: UnixMillis,
}

/// Everything needed to register (or rotate) one subscription.
pub struct NewWebhookSubscription<'a> {
    /// Subscription id (unique per tenant).
    pub id: &'a str,
    /// Owning tenant.
    pub tenant: &'a TenantId,
    /// Delivery endpoint (validated by the caller, e.g. the F018 SSRF
    /// guard, before it reaches storage).
    pub url: &'a str,
    /// The cipher whose master key seals the signing secret.
    pub cipher: &'a CredentialCipher,
    /// The HMAC signing secret (sealed here, before the database).
    pub secret: SecretBox,
    /// The event type to receive (`None` = all).
    pub event_type: Option<&'a str>,
    /// Registration instant.
    pub at: UnixMillis,
}

/// One queued delivery: a snapshot of the event and the exact body it
/// will be POSTed as.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeliveryRecord {
    /// Delivery id.
    pub id: String,
    /// Owning tenant.
    pub tenant_id: TenantId,
    /// The subscription it goes to.
    pub subscription_id: String,
    /// The event it carries.
    pub event_id: EventId,
    /// The event's log sequence (for ordering and gap bookkeeping).
    pub event_seq: u64,
    /// The exact JSON body to POST (immutable snapshot).
    pub body: String,
    /// Delivery attempts so far.
    pub attempts: u32,
    /// Lifecycle position.
    pub status: DeliveryStatus,
    /// Earliest instant the next attempt may run.
    pub next_attempt_at: UnixMillis,
    /// The last transport outcome text (secret-free by contract).
    pub last_error: Option<String>,
    /// Creation instant.
    pub created_at: UnixMillis,
    /// Last mutation instant.
    pub updated_at: UnixMillis,
}

/// A delivery to enqueue.
pub struct NewDelivery<'a> {
    /// Delivery id (unique per tenant).
    pub id: &'a str,
    /// Owning tenant.
    pub tenant: &'a TenantId,
    /// Target subscription.
    pub subscription_id: &'a str,
    /// The event being delivered.
    pub event_id: &'a EventId,
    /// The event's log sequence.
    pub event_seq: u64,
    /// The exact JSON body to POST.
    pub body: &'a str,
    /// Enqueue instant (also the first `next_attempt_at`).
    pub at: UnixMillis,
}

/// The sealed-secret column set, shared by write and read paths.
/// Column names follow the F008 convention (`secret_nonce`/`secret_cipher`
/// hold the *data* envelope; `dek_nonce`/`wrapped_dek` the DEK wrap).
struct SecretColumns {
    secret_nonce: Vec<u8>,
    secret_cipher: Vec<u8>,
    dek_nonce: Vec<u8>,
    wrapped_dek: Vec<u8>,
    kek_version: i32,
    fingerprint: Option<String>,
}

fn seal_secret(cipher: &CredentialCipher, secret: &SecretBox) -> StorageResult<SecretColumns> {
    let sealed: SealedSecret = cipher.seal(secret.expose())?;
    Ok(SecretColumns {
        secret_nonce: sealed.data_nonce,
        secret_cipher: sealed.data_cipher,
        dek_nonce: sealed.dek_nonce,
        wrapped_dek: sealed.wrapped_dek,
        kek_version: sealed.kek_version,
        fingerprint: Some(secret.fingerprint()),
    })
}

impl Store {
    /// Registers a subscription, sealing its signing secret before the
    /// database sees it. Re-registering the same id rotates the URL,
    /// secret and event filter; `is_enabled` is left to
    /// [`Store::set_webhook_enabled`].
    pub async fn upsert_webhook_subscription(
        &self,
        sub: &NewWebhookSubscription<'_>,
    ) -> StorageResult<WebhookSubscriptionRecord> {
        let sealed = seal_secret(sub.cipher, &sub.secret)?;
        let mut tx = self.begin_tenant(sub.tenant).await?;
        let row = sqlx::query(
            "INSERT INTO webhook_subscriptions
                (id, tenant_id, url, secret_nonce, secret_cipher, dek_nonce, wrapped_dek,
                 kek_version, fingerprint, event_type, is_enabled, created_at, updated_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, TRUE, $11, $11)
             ON CONFLICT (tenant_id, id) DO UPDATE SET
                url = EXCLUDED.url,
                secret_nonce = EXCLUDED.secret_nonce,
                secret_cipher = EXCLUDED.secret_cipher,
                dek_nonce = EXCLUDED.dek_nonce,
                wrapped_dek = EXCLUDED.wrapped_dek,
                kek_version = EXCLUDED.kek_version,
                fingerprint = EXCLUDED.fingerprint,
                event_type = EXCLUDED.event_type,
                updated_at = EXCLUDED.updated_at
             RETURNING id, tenant_id, url, kek_version, fingerprint, event_type,
                       is_enabled, created_at, updated_at",
        )
        .bind(sub.id)
        .bind(sub.tenant.as_str())
        .bind(sub.url)
        .bind(&sealed.secret_nonce)
        .bind(&sealed.secret_cipher)
        .bind(&sealed.dek_nonce)
        .bind(&sealed.wrapped_dek)
        .bind(sealed.kek_version)
        .bind(sealed.fingerprint)
        .bind(sub.event_type)
        .bind(sub.at.as_millis())
        .fetch_one(&mut *tx)
        .await?;
        tx.commit().await?;
        map_subscription(&row)
    }

    /// Lists the tenant's subscriptions (metadata only — no secret
    /// material exists outside the sealed columns).
    pub async fn list_webhook_subscriptions(
        &self,
        tenant: &TenantId,
    ) -> StorageResult<Vec<WebhookSubscriptionRecord>> {
        let mut tx = self.begin_tenant(tenant).await?;
        let rows = sqlx::query(
            "SELECT id, tenant_id, url, kek_version, fingerprint, event_type,
                    is_enabled, created_at, updated_at
             FROM webhook_subscriptions ORDER BY created_at, id",
        )
        .bind(tenant.as_str())
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;
        rows.iter().map(map_subscription).collect()
    }

    /// Enables or disables a subscription; disabled subscriptions stay
    /// readable but receive no fan-out.
    pub async fn set_webhook_enabled(
        &self,
        tenant: &TenantId,
        id: &str,
        enabled: bool,
        at: UnixMillis,
    ) -> StorageResult<()> {
        let mut tx = self.begin_tenant(tenant).await?;
        let updated = sqlx::query(
            "UPDATE webhook_subscriptions SET is_enabled = $3, updated_at = $4
             WHERE id = $2",
        )
        .bind(tenant.as_str())
        .bind(id)
        .bind(enabled)
        .bind(at.as_millis())
        .execute(&mut *tx)
        .await?
        .rows_affected();
        tx.commit().await?;
        if updated == 0 {
            return Err(StorageError::NotFound {
                what: "webhook subscription",
                id: id.to_owned(),
            });
        }
        Ok(())
    }

    /// Removes a subscription. Past deliveries stay (they are the
    /// delivery audit trail); queued ones are abandoned by the caller
    /// seeing the subscription gone.
    pub async fn delete_webhook_subscription(
        &self,
        tenant: &TenantId,
        id: &str,
    ) -> StorageResult<()> {
        let mut tx = self.begin_tenant(tenant).await?;
        let deleted = sqlx::query("DELETE FROM webhook_subscriptions WHERE id = $2")
            .bind(tenant.as_str())
            .bind(id)
            .execute(&mut *tx)
            .await?
            .rows_affected();
        tx.commit().await?;
        if deleted == 0 {
            return Err(StorageError::NotFound {
                what: "webhook subscription",
                id: id.to_owned(),
            });
        }
        Ok(())
    }

    /// Unseals a subscription's signing secret for one signing
    /// operation. The secret never outlives the returned [`SecretBox`].
    pub async fn open_webhook_secret(
        &self,
        tenant: &TenantId,
        id: &str,
        cipher: &CredentialCipher,
    ) -> StorageResult<SecretBox> {
        let mut tx = self.begin_tenant(tenant).await?;
        let row = sqlx::query(
            "SELECT secret_nonce, secret_cipher, dek_nonce, wrapped_dek, kek_version
             FROM webhook_subscriptions WHERE id = $2",
        )
        .bind(tenant.as_str())
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(|| StorageError::NotFound {
            what: "webhook subscription",
            id: id.to_owned(),
        })?;
        tx.commit().await?;
        let sealed = SealedSecret {
            kek_version: row.try_get("kek_version")?,
            dek_nonce: row.try_get("dek_nonce")?,
            wrapped_dek: row.try_get("wrapped_dek")?,
            data_nonce: row.try_get("secret_nonce")?,
            data_cipher: row.try_get("secret_cipher")?,
        };
        cipher.open(&sealed)
    }

    /// Queues one delivery.
    pub async fn insert_delivery(&self, d: &NewDelivery<'_>) -> StorageResult<DeliveryRecord> {
        if d.body.trim().is_empty() {
            return Err(StorageError::Invalid {
                what: "delivery body",
                reason: "must not be empty".to_owned(),
            });
        }
        let mut tx = self.begin_tenant(d.tenant).await?;
        let row = sqlx::query(
            "INSERT INTO webhook_deliveries
                (id, tenant_id, subscription_id, event_id, event_seq, body,
                 attempts, status, next_attempt_at, created_at, updated_at)
             VALUES ($1, $2, $3, $4, $5, $6, 0, 'pending', $7, $7, $7)
             RETURNING *",
        )
        .bind(d.id)
        .bind(d.tenant.as_str())
        .bind(d.subscription_id)
        .bind(d.event_id.as_str())
        .bind(
            i64::try_from(d.event_seq).map_err(|_| StorageError::Invalid {
                what: "event seq",
                reason: format!("{} exceeds the sequence range", d.event_seq),
            })?,
        )
        .bind(d.body)
        .bind(d.at.as_millis())
        .fetch_one(&mut *tx)
        .await?;
        tx.commit().await?;
        map_delivery(&row)
    }

    /// Claims due deliveries atomically: `pending` rows whose
    /// `next_attempt_at` has come, plus `delivering` rows stale for
    /// `stale_after_ms` (a crashed loop's leftovers), flip to
    /// `delivering` and are returned oldest-first.
    pub async fn claim_due_deliveries(
        &self,
        tenant: &TenantId,
        now: UnixMillis,
        stale_after_ms: i64,
        limit: u32,
    ) -> StorageResult<Vec<DeliveryRecord>> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let mut tx = self.begin_tenant(tenant).await?;
        let rows = sqlx::query(
            "UPDATE webhook_deliveries SET status = 'delivering', updated_at = $3
             WHERE id IN (
                SELECT id FROM webhook_deliveries
                WHERE tenant_id = $1 AND (
                    (status = 'pending' AND next_attempt_at <= $3)
                    OR (status = 'delivering' AND updated_at <= $2)
                )
                ORDER BY next_attempt_at, id
                LIMIT $4
             )
             RETURNING *",
        )
        .bind(tenant.as_str())
        .bind(now.as_millis() - stale_after_ms)
        .bind(now.as_millis())
        .bind(i64::from(limit))
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;
        rows.iter().map(map_delivery).collect()
    }

    /// Marks a claimed delivery as accepted by the transport.
    pub async fn settle_delivered(
        &self,
        tenant: &TenantId,
        id: &str,
        at: UnixMillis,
    ) -> StorageResult<()> {
        let mut tx = self.begin_tenant(tenant).await?;
        let updated = sqlx::query(
            "UPDATE webhook_deliveries SET status = 'delivered', last_error = NULL,
                    updated_at = $3
             WHERE id = $2",
        )
        .bind(tenant.as_str())
        .bind(id)
        .bind(at.as_millis())
        .execute(&mut *tx)
        .await?
        .rows_affected();
        tx.commit().await?;
        if updated == 0 {
            return Err(StorageError::NotFound {
                what: "webhook delivery",
                id: id.to_owned(),
            });
        }
        Ok(())
    }

    /// Puts a claimed delivery back to `pending` with the retry
    /// schedule's next due instant and the transport's outcome text.
    pub async fn settle_retry(
        &self,
        tenant: &TenantId,
        id: &str,
        attempts: u32,
        next_attempt_at: UnixMillis,
        error: Option<&str>,
        at: UnixMillis,
    ) -> StorageResult<()> {
        let mut tx = self.begin_tenant(tenant).await?;
        let updated = sqlx::query(
            "UPDATE webhook_deliveries SET status = 'pending', attempts = $3,
                    next_attempt_at = $4, last_error = $5, updated_at = $6
             WHERE id = $2",
        )
        .bind(tenant.as_str())
        .bind(id)
        .bind(i64::from(attempts))
        .bind(next_attempt_at.as_millis())
        .bind(error)
        .bind(at.as_millis())
        .execute(&mut *tx)
        .await?
        .rows_affected();
        tx.commit().await?;
        if updated == 0 {
            return Err(StorageError::NotFound {
                what: "webhook delivery",
                id: id.to_owned(),
            });
        }
        Ok(())
    }

    /// Marks a claimed delivery abandoned (permanent fault or exhausted
    /// retry budget); only [`Store::replay_delivery`] revives it.
    pub async fn settle_abandoned(
        &self,
        tenant: &TenantId,
        id: &str,
        error: Option<&str>,
        at: UnixMillis,
    ) -> StorageResult<()> {
        let mut tx = self.begin_tenant(tenant).await?;
        let updated = sqlx::query(
            "UPDATE webhook_deliveries SET status = 'abandoned', last_error = $3,
                    updated_at = $4
             WHERE id = $2",
        )
        .bind(tenant.as_str())
        .bind(id)
        .bind(error)
        .bind(at.as_millis())
        .execute(&mut *tx)
        .await?
        .rows_affected();
        tx.commit().await?;
        if updated == 0 {
            return Err(StorageError::NotFound {
                what: "webhook delivery",
                id: id.to_owned(),
            });
        }
        Ok(())
    }

    /// Manual redelivery (重投): resets a delivery to `pending` with a
    /// fresh retry budget, due immediately. Works from any status.
    pub async fn replay_delivery(
        &self,
        tenant: &TenantId,
        id: &str,
        now: UnixMillis,
    ) -> StorageResult<DeliveryRecord> {
        let mut tx = self.begin_tenant(tenant).await?;
        let row = sqlx::query(
            "UPDATE webhook_deliveries SET status = 'pending', attempts = 0,
                    next_attempt_at = $3, last_error = NULL, updated_at = $3
             WHERE id = $2
             RETURNING *",
        )
        .bind(tenant.as_str())
        .bind(id)
        .bind(now.as_millis())
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(|| StorageError::NotFound {
            what: "webhook delivery",
            id: id.to_owned(),
        })?;
        tx.commit().await?;
        map_delivery(&row)
    }
}

fn map_subscription(row: &sqlx::postgres::PgRow) -> StorageResult<WebhookSubscriptionRecord> {
    Ok(WebhookSubscriptionRecord {
        id: row.try_get("id")?,
        tenant_id: TenantId::parse(row.try_get::<String, _>("tenant_id")?)?,
        url: row.try_get("url")?,
        kek_version: row.try_get("kek_version")?,
        fingerprint: row.try_get("fingerprint")?,
        event_type: row.try_get("event_type")?,
        is_enabled: row.try_get("is_enabled")?,
        created_at: UnixMillis::from_millis(row.try_get::<i64, _>("created_at")?)?,
        updated_at: UnixMillis::from_millis(row.try_get::<i64, _>("updated_at")?)?,
    })
}

fn map_delivery(row: &sqlx::postgres::PgRow) -> StorageResult<DeliveryRecord> {
    let status_raw: String = row.try_get("status")?;
    Ok(DeliveryRecord {
        id: row.try_get("id")?,
        tenant_id: TenantId::parse(row.try_get::<String, _>("tenant_id")?)?,
        subscription_id: row.try_get("subscription_id")?,
        event_id: EventId::parse(row.try_get::<String, _>("event_id")?)?,
        event_seq: u64::try_from(row.try_get::<i64, _>("event_seq")?).map_err(|_| {
            StorageError::Invalid {
                what: "event seq",
                reason: "stored sequence is negative".to_owned(),
            }
        })?,
        body: row.try_get("body")?,
        attempts: u32::try_from(row.try_get::<i32, _>("attempts")?).map_err(|_| {
            StorageError::Invalid {
                what: "delivery attempts",
                reason: "stored count is negative".to_owned(),
            }
        })?,
        status: DeliveryStatus::parse(&status_raw)?,
        next_attempt_at: UnixMillis::from_millis(row.try_get::<i64, _>("next_attempt_at")?)?,
        last_error: row.try_get("last_error")?,
        created_at: UnixMillis::from_millis(row.try_get::<i64, _>("created_at")?)?,
        updated_at: UnixMillis::from_millis(row.try_get::<i64, _>("updated_at")?)?,
    })
}
