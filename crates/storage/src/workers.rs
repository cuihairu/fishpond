//! Worker registry (F012): registration, heartbeat and liveness.
//!
//! A worker registers on startup and heartbeats while it runs. Re-registration
//! refreshes name/capabilities and the heartbeat but keeps the original
//! `registered_at`, so the row doubles as the worker's uptime record. A worker
//! whose heartbeat is older than a cutoff counts as *lost* — detection is a
//! plain query here; acting on it is the scheduler's job (F013).

use fishpond_domain::{TenantId, UnixMillis, WorkerId};
use sqlx::Row;

use crate::error::{StorageError, StorageResult};
use crate::store::Store;

/// One registered worker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkerRecord {
    /// Worker id.
    pub id: WorkerId,
    /// Owning tenant.
    pub tenant_id: TenantId,
    /// Display name (e.g. hostname).
    pub name: String,
    /// Declared capabilities (e.g. `["browser", "xianyu"]`).
    pub capabilities: Vec<String>,
    /// First registration instant (kept across re-registrations).
    pub registered_at: UnixMillis,
    /// Last heartbeat instant.
    pub last_heartbeat_at: UnixMillis,
    /// Last row update instant.
    pub updated_at: UnixMillis,
}

/// A worker registration.
pub struct NewWorker<'a> {
    /// Worker id.
    pub id: &'a WorkerId,
    /// Owning tenant.
    pub tenant: &'a TenantId,
    /// Display name.
    pub name: &'a str,
    /// Declared capabilities.
    pub capabilities: &'a [&'a str],
    /// Registration instant (also the initial heartbeat).
    pub at: UnixMillis,
}

impl Store {
    /// Registers a worker, or re-registers an existing one (refreshing
    /// name, capabilities and heartbeat, keeping the original
    /// `registered_at`).
    pub async fn register_worker(&self, worker: &NewWorker<'_>) -> StorageResult<WorkerRecord> {
        let mut tx = self.begin_tenant(worker.tenant).await?;
        let row = sqlx::query(
            "INSERT INTO workers
                 (id, tenant_id, name, capabilities, registered_at, last_heartbeat_at, updated_at)
             VALUES ($1, $2, $3, $4, $5, $5, $5)
             ON CONFLICT (id) DO UPDATE SET
                 name = $3,
                 capabilities = $4,
                 last_heartbeat_at = $5,
                 updated_at = $5
             RETURNING id, tenant_id, name, capabilities, registered_at,
                       last_heartbeat_at, updated_at",
        )
        .bind(worker.id.as_str())
        .bind(worker.tenant.as_str())
        .bind(worker.name)
        .bind(worker.capabilities)
        .bind(worker.at.as_millis())
        .fetch_one(&mut *tx)
        .await?;
        tx.commit().await?;
        map_worker(&row)
    }

    /// Advances the worker's heartbeat; `Ok(false)` when the worker is
    /// not registered (the caller decides whether that is fatal).
    pub async fn heartbeat_worker(
        &self,
        tenant: &TenantId,
        id: &WorkerId,
        at: UnixMillis,
    ) -> StorageResult<bool> {
        let mut tx = self.begin_tenant(tenant).await?;
        let result = sqlx::query(
            "UPDATE workers SET last_heartbeat_at = $3, updated_at = $3
             WHERE id = $1 AND tenant_id = $2",
        )
        .bind(id.as_str())
        .bind(tenant.as_str())
        .bind(at.as_millis())
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(result.rows_affected() == 1)
    }

    /// Returns one registered worker, or
    /// [`StorageError::NotFound`].
    pub async fn get_worker(
        &self,
        tenant: &TenantId,
        id: &WorkerId,
    ) -> StorageResult<WorkerRecord> {
        let mut tx = self.begin_tenant(tenant).await?;
        let row = sqlx::query(
            "SELECT id, tenant_id, name, capabilities, registered_at,
                    last_heartbeat_at, updated_at
             FROM workers WHERE id = $1",
        )
        .bind(id.as_str())
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(|| StorageError::NotFound {
            what: "worker",
            id: id.as_str().to_owned(),
        })?;
        tx.commit().await?;
        map_worker(&row)
    }

    /// Lists every registered worker of `tenant`, oldest registration
    /// first.
    pub async fn list_workers(&self, tenant: &TenantId) -> StorageResult<Vec<WorkerRecord>> {
        let mut tx = self.begin_tenant(tenant).await?;
        let rows = sqlx::query(
            "SELECT id, tenant_id, name, capabilities, registered_at,
                    last_heartbeat_at, updated_at
             FROM workers ORDER BY registered_at, id",
        )
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;
        rows.iter().map(map_worker).collect()
    }

    /// Workers whose last heartbeat is at or before `stale_before` —
    /// the *lost* set (idempotent to re-read; no state is changed).
    pub async fn lost_workers(
        &self,
        tenant: &TenantId,
        stale_before: UnixMillis,
    ) -> StorageResult<Vec<WorkerRecord>> {
        let mut tx = self.begin_tenant(tenant).await?;
        let rows = sqlx::query(
            "SELECT id, tenant_id, name, capabilities, registered_at,
                    last_heartbeat_at, updated_at
             FROM workers
             WHERE last_heartbeat_at <= $1
             ORDER BY last_heartbeat_at, id",
        )
        .bind(stale_before.as_millis())
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;
        rows.iter().map(map_worker).collect()
    }

    /// Removes the worker's registration; `Ok(false)` when unknown.
    /// Leases it still holds are *not* touched — they expire and are
    /// stolen on the normal CAS path.
    pub async fn deregister_worker(&self, tenant: &TenantId, id: &WorkerId) -> StorageResult<bool> {
        let mut tx = self.begin_tenant(tenant).await?;
        let result = sqlx::query("DELETE FROM workers WHERE id = $1 AND tenant_id = $2")
            .bind(id.as_str())
            .bind(tenant.as_str())
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(result.rows_affected() == 1)
    }
}

fn map_worker(row: &sqlx::postgres::PgRow) -> StorageResult<WorkerRecord> {
    Ok(WorkerRecord {
        id: WorkerId::parse(row.try_get::<String, _>("id")?)?,
        tenant_id: TenantId::parse(row.try_get::<String, _>("tenant_id")?)?,
        name: row.try_get("name")?,
        capabilities: row.try_get("capabilities")?,
        registered_at: UnixMillis::from_millis(row.try_get::<i64, _>("registered_at")?)?,
        last_heartbeat_at: UnixMillis::from_millis(row.try_get::<i64, _>("last_heartbeat_at")?)?,
        updated_at: UnixMillis::from_millis(row.try_get::<i64, _>("updated_at")?)?,
    })
}
