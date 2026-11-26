//! Worker registration and heartbeat (F012, acceptance surface T01).
//!
//! The service wraps the storage registry: register on startup, heartbeat
//! on an interval (chosen by the daemon loop, later milestones), and
//! treat "heartbeat says unknown" as the signal to re-register. Liveness
//! *detection* lives in [`Registration::lost`]; acting on lost workers is
//! the scheduler's job (F013).

use fishpond_domain::{TenantId, UnixMillis, WorkerId};
use fishpond_storage::{NewWorker, StorageResult, Store, WorkerRecord};

/// The registration service.
#[derive(Clone)]
pub struct Registration {
    store: Store,
}

impl Registration {
    /// Wraps an opened [`Store`].
    pub fn new(store: Store) -> Self {
        Self { store }
    }

    /// Registers (or re-registers) the worker. Re-registration refreshes
    /// name, capabilities and heartbeat; the original `registered_at`
    /// survives, so the row doubles as the worker's uptime record.
    pub async fn register(
        &self,
        tenant: &TenantId,
        id: WorkerId,
        name: &str,
        capabilities: &[&str],
        at: UnixMillis,
    ) -> StorageResult<WorkerRecord> {
        self.store
            .register_worker(&NewWorker {
                id: &id,
                tenant,
                name,
                capabilities,
                at,
            })
            .await
    }

    /// Advances the heartbeat. An unknown worker surfaces as
    /// [`fishpond_storage::StorageError::NotFound`] — the daemon answers
    /// that by registering again.
    pub async fn heartbeat(
        &self,
        tenant: &TenantId,
        id: &WorkerId,
        at: UnixMillis,
    ) -> StorageResult<()> {
        if self.store.heartbeat_worker(tenant, id, at).await? {
            Ok(())
        } else {
            Err(fishpond_storage::StorageError::NotFound {
                what: "worker",
                id: id.as_str().to_owned(),
            })
        }
    }

    /// One registered worker.
    pub async fn get(&self, tenant: &TenantId, id: &WorkerId) -> StorageResult<WorkerRecord> {
        self.store.get_worker(tenant, id).await
    }

    /// Every registered worker of `tenant`.
    pub async fn list(&self, tenant: &TenantId) -> StorageResult<Vec<WorkerRecord>> {
        self.store.list_workers(tenant).await
    }

    /// Workers whose heartbeat stopped before `stale_before`.
    pub async fn lost(
        &self,
        tenant: &TenantId,
        stale_before: UnixMillis,
    ) -> StorageResult<Vec<WorkerRecord>> {
        self.store.lost_workers(tenant, stale_before).await
    }

    /// Removes the registration; leases it still holds expire and are
    /// stolen on the normal CAS path.
    pub async fn deregister(&self, tenant: &TenantId, id: &WorkerId) -> StorageResult<bool> {
        self.store.deregister_worker(tenant, id).await
    }
}
