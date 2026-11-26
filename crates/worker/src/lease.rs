//! Account leases (F012, acceptance surfaces T02 and T20).
//!
//! A thin, typed facade over the storage CAS: [`LeaseManager::acquire`]
//! is the compare-and-set entry (fresh take, steal of an expired term,
//! or re-extension of the worker's own), [`LeaseManager::renew`] is the
//! holder-only keep-alive whose failure is the fencing signal that the
//! worker lost the account, and [`LeaseManager::release`] hands the
//! account back. Daemon-side loops (renew on interval, steal-back
//! policy) belong to the scheduler milestones, not here.

use fishpond_domain::{AccountId, TenantId, UnixMillis, WorkerId};
use fishpond_storage::{AccountLease, StorageResult, Store};

/// The lease service.
#[derive(Clone)]
pub struct LeaseManager {
    store: Store,
}

impl LeaseManager {
    /// Wraps an opened [`Store`].
    pub fn new(store: Store) -> Self {
        Self { store }
    }

    /// Acquires the lease on `account` until `expires_at`. Conflicts
    /// name the live holder and its generation, so the caller can tell
    /// "someone else owns this account right now" from storage faults.
    pub async fn acquire(
        &self,
        tenant: &TenantId,
        account: &AccountId,
        holder: &WorkerId,
        expires_at: UnixMillis,
        at: UnixMillis,
    ) -> StorageResult<AccountLease> {
        self.store
            .acquire_lease(tenant, account, holder, expires_at, at)
            .await
    }

    /// Extends the worker's own term. An error here means the lease was
    /// lost (stolen after expiry) or already expired — either way the
    /// worker must stop acting on the account and re-acquire.
    pub async fn renew(
        &self,
        tenant: &TenantId,
        account: &AccountId,
        holder: &WorkerId,
        expires_at: UnixMillis,
        at: UnixMillis,
    ) -> StorageResult<AccountLease> {
        self.store
            .renew_lease(tenant, account, holder, expires_at, at)
            .await
    }

    /// Releases the worker's own lease; `false` when it is not the
    /// holder (the fencing signal already fired, or nothing was held).
    pub async fn release(
        &self,
        tenant: &TenantId,
        account: &AccountId,
        holder: &WorkerId,
    ) -> StorageResult<bool> {
        self.store.release_lease(tenant, account, holder).await
    }

    /// The current lease on `account`, if any.
    pub async fn get(
        &self,
        tenant: &TenantId,
        account: &AccountId,
    ) -> StorageResult<Option<AccountLease>> {
        self.store.get_lease(tenant, account).await
    }
}
