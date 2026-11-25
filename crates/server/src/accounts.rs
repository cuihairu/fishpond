//! Account management and the status API (F009).
//!
//! Reads are tenant-scoped projections of the persisted rows; state
//! changes run the domain [`AccountState`] machine so an observation
//! loop cannot silently no-op or teleport an account between states.
//! `UNKNOWN` is a real state surfaced as-is — reconcile (F015) is the
//! only thing that resolves it.

use crate::error::{ServiceError, ServiceResult};
use fishpond_domain::{Account, AccountId, AccountState, TenantId, UnixMillis};

/// Shared, cloneable handle onto the persistence layer.
#[derive(Clone)]
pub struct Accounts {
    store: fishpond_storage::Store,
}

impl Accounts {
    /// Wraps an opened [`Store`].
    pub fn new(store: fishpond_storage::Store) -> Self {
        Self { store }
    }

    /// Registers one account, starting in the `OFFLINE` state. Fails with
    /// [`ServiceError::Conflict`] when the id already exists — register
    /// is the only way an account is created, and overwriting a state a
    /// reconcile already confirmed would hide the disagreement.
    pub async fn register(
        &self,
        tenant: &TenantId,
        id: AccountId,
        platform: &str,
        at: UnixMillis,
    ) -> ServiceResult<Account> {
        if self.exists(tenant, &id).await? {
            return Err(ServiceError::conflict(
                "account",
                id.as_str(),
                "already exists; use state updates instead",
            ));
        }
        let account = Account::new(
            id,
            tenant.clone(),
            platform,
            AccountState::Offline,
            None,
            at,
        )?;
        self.store.upsert_account(&account).await?;
        Ok(account)
    }

    /// Returns one account of `tenant`, or [`ServiceError::NotFound`].
    pub async fn get(&self, tenant: &TenantId, id: &AccountId) -> ServiceResult<Account> {
        self.store.get_account(tenant, id).await.map_err(Into::into)
    }

    /// Whether an account id exists for the tenant.
    pub async fn exists(&self, tenant: &TenantId, id: &AccountId) -> ServiceResult<bool> {
        Ok(self.get(tenant, id).await.is_ok())
    }

    /// Lists every account of the tenant (the wire shape paginates in
    /// F025; the service returns the full list and the caller slices).
    pub async fn list(&self, tenant: &TenantId) -> ServiceResult<Vec<Account>> {
        self.store.list_accounts(tenant).await.map_err(Into::into)
    }

    /// Reports state counts per [`AccountState`] — the status API's
    /// aggregate, always covering every state (zeros included) so a
    /// dashboard cannot confuse "no accounts" with "missing state".
    pub async fn status(&self, tenant: &TenantId) -> ServiceResult<StatusSummary> {
        let accounts = self.list(tenant).await?;
        let mut summary = StatusSummary::default();
        for account in &accounts {
            match account.state {
                AccountState::Online => summary.online += 1,
                AccountState::Offline => summary.offline += 1,
                AccountState::Unknown => summary.unknown += 1,
                AccountState::Disabled => summary.disabled += 1,
            }
        }
        summary.total = accounts.len();
        Ok(summary)
    }

    /// Applies one state change, enforcing the domain machine. The
    /// account must exist; a same-state "update" is rejected by the
    /// machine itself (staying put is not an observation), so callers
    /// must only call this when something actually changed.
    pub async fn set_state(
        &self,
        tenant: &TenantId,
        id: &AccountId,
        next: AccountState,
        note: Option<String>,
        at: UnixMillis,
    ) -> ServiceResult<Account> {
        let account = self.get(tenant, id).await?;
        let transitioned = account.state.transition_to(next)?;
        let updated = self
            .store
            .update_account_state(tenant, id, transitioned, note, at)
            .await?;
        Ok(updated)
    }
}

/// Aggregate account counts by state (see [`Accounts::status`]).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StatusSummary {
    /// Confirmed online accounts.
    pub online: usize,
    /// Confirmed offline accounts.
    pub offline: usize,
    /// State could not be verified (never silently zeroed by absence).
    pub unknown: usize,
    /// Administratively disabled accounts.
    pub disabled: usize,
    /// Total accounts (`online + offline + unknown + disabled`).
    pub total: usize,
}
