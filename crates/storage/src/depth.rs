//! Queue-depth probes for the telemetry plane (F030): read-only counts
//! over operations, attempts and consumer lag. Every query is
//! tenant-bound via [`Store::begin_tenant`] with row-level security as
//! the second isolation layer.

use fishpond_domain::TenantId;
use sqlx::Row;

use crate::error::StorageResult;
use crate::store::Store;

impl Store {
    /// Per-state operation counts, ordered by state name — the backlog
    /// face of the operations queue.
    pub async fn operation_state_counts(
        &self,
        tenant: &TenantId,
    ) -> StorageResult<Vec<(String, i64)>> {
        let mut tx = self.begin_tenant(tenant).await?;
        let rows = sqlx::query(
            "SELECT state, COUNT(*) AS n FROM operations GROUP BY state ORDER BY state",
        )
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;
        rows.iter()
            .map(|row| {
                Ok((
                    row.try_get::<String, _>("state")?,
                    row.try_get::<i64, _>("n")?,
                ))
            })
            .collect()
    }

    /// How many attempts are in flight (no outcome yet) for `tenant`.
    pub async fn in_flight_attempts(&self, tenant: &TenantId) -> StorageResult<i64> {
        let mut tx = self.begin_tenant(tenant).await?;
        let (n,): (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM operation_attempts WHERE outcome IS NULL")
                .fetch_one(&mut *tx)
                .await?;
        tx.commit().await?;
        Ok(n)
    }

    /// How many in-flight attempts have blown their deadline — the
    /// reaper's backlog, an alert source.
    pub async fn overdue_attempts(&self, tenant: &TenantId, now_ms: i64) -> StorageResult<i64> {
        let mut tx = self.begin_tenant(tenant).await?;
        let (n,): (i64,) = sqlx::query_as(
            "SELECT COUNT(*) FROM operation_attempts
             WHERE outcome IS NULL AND deadline_at < $2",
        )
        .bind(tenant.as_str())
        .bind(now_ms)
        .fetch_one(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(n)
    }

    /// How far `consumer` trails the event head: `head_seq - checkpoint`.
    /// Zero with no events and zero with a caught-up consumer.
    pub async fn consumer_lag(&self, tenant: &TenantId, consumer: &str) -> StorageResult<u64> {
        let mut tx = self.begin_tenant(tenant).await?;
        let head: Option<i64> = sqlx::query_scalar("SELECT MAX(seq) FROM events")
            .fetch_one(&mut *tx)
            .await?;
        let last: Option<i64> =
            sqlx::query_scalar("SELECT last_seq FROM projection_checkpoints WHERE consumer = $2")
                .bind(tenant.as_str())
                .bind(consumer)
                .fetch_optional(&mut *tx)
                .await?;
        tx.commit().await?;
        let head = head.map_or(0, |v| u64::try_from(v).unwrap_or(0));
        let checkpoint = last.map_or(0, |v| u64::try_from(v).unwrap_or(0));
        Ok(head.saturating_sub(checkpoint))
    }
}
