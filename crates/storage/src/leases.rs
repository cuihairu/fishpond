//! Account leases (F012): the per-account exclusive automation right,
//! with CAS acquire/renew/steal and generation fencing.
//!
//! At most one worker may hold a live lease on an account. Every state
//! change is a single compare-and-set statement, so concurrent acquires
//! resolve without an application-side lock:
//!
//! * **acquire** — inserts, or takes over an *expired* lease (steal) or
//!   re-extends the worker's own. A live foreign lease rejects the CAS.
//! * **renew** — holder-only, and only while the term is still alive:
//!   an expired term can never be silently extended; the worker must
//!   re-acquire (which loses against a steal that happened first).
//! * **release** — holder-only; anyone else's release is a no-op.
//!
//! `generation` is the fencing token: it increments only when the holder
//! changes (steal), never on renewal. A worker that paused past its
//! expiry detects the loss through a failed renew and re-reads the
//! generation to learn it is no longer the holder.

use fishpond_domain::{AccountId, TenantId, UnixMillis, WorkerId};
use sqlx::Row;

use crate::error::{StorageError, StorageResult};
use crate::store::Store;

/// One account lease term.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountLease {
    /// Owning tenant.
    pub tenant_id: TenantId,
    /// The leased account.
    pub account_id: AccountId,
    /// The holding worker.
    pub holder: WorkerId,
    /// Fencing token; increments on steal, unchanged on renewal.
    pub generation: i64,
    /// When the current term ends.
    pub expires_at: UnixMillis,
    /// When the current term began.
    pub acquired_at: UnixMillis,
}

impl Store {
    /// Acquires the lease on `account` for `holder` until `expires_at`.
    /// Succeeds when the account is unleased, the lease is expired
    /// (steal: generation increments) or already held by `holder`
    /// (re-extend: generation unchanged). A live foreign lease is a
    /// [`StorageError::Conflict`] naming the holder and generation.
    pub async fn acquire_lease(
        &self,
        tenant: &TenantId,
        account: &AccountId,
        holder: &WorkerId,
        expires_at: UnixMillis,
        at: UnixMillis,
    ) -> StorageResult<AccountLease> {
        let mut tx = self.begin_tenant(tenant).await?;
        let row = sqlx::query(
            "INSERT INTO account_leases
                 (tenant_id, account_id, holder, generation, expires_at, acquired_at)
             VALUES ($1, $2, $3, 1, $4, $5)
             ON CONFLICT (tenant_id, account_id) DO UPDATE SET
                 holder = $3,
                 generation = CASE WHEN account_leases.holder = $3
                                   THEN account_leases.generation
                                   ELSE account_leases.generation + 1 END,
                 expires_at = $4,
                 acquired_at = $5
             WHERE account_leases.holder = $3 OR account_leases.expires_at <= $5
             RETURNING tenant_id, account_id, holder, generation, expires_at, acquired_at",
        )
        .bind(tenant.as_str())
        .bind(account.as_str())
        .bind(holder.as_str())
        .bind(expires_at.as_millis())
        .bind(at.as_millis())
        .fetch_optional(&mut *tx)
        .await?;
        tx.commit().await?;
        match row {
            Some(row) => map_lease(&row),
            None => {
                // The CAS lost, which can only mean a live foreign
                // holder; surface who, for the fencing diagnostics.
                let current = self.get_lease(tenant, account).await?.ok_or_else(|| {
                    StorageError::Conflict {
                        what: "account lease",
                        id: account.as_str().to_owned(),
                        reason: "lease vanished while acquiring".to_owned(),
                    }
                })?;
                Err(StorageError::Conflict {
                    what: "account lease",
                    id: account.as_str().to_owned(),
                    reason: format!(
                        "held by {} (generation {})",
                        current.holder, current.generation
                    ),
                })
            }
        }
    }

    /// Extends the worker's own lease to `expires_at`; the generation is
    /// unchanged. Fails when the worker is not the holder, or the term
    /// already expired (re-acquire instead — that is the fencing
    /// discipline: expired terms are never extended in place).
    pub async fn renew_lease(
        &self,
        tenant: &TenantId,
        account: &AccountId,
        holder: &WorkerId,
        expires_at: UnixMillis,
        at: UnixMillis,
    ) -> StorageResult<AccountLease> {
        let mut tx = self.begin_tenant(tenant).await?;
        let row = sqlx::query(
            "UPDATE account_leases SET expires_at = $4
             WHERE tenant_id = $1 AND account_id = $2
                   AND holder = $3 AND expires_at > $5
             RETURNING tenant_id, account_id, holder, generation, expires_at, acquired_at",
        )
        .bind(tenant.as_str())
        .bind(account.as_str())
        .bind(holder.as_str())
        .bind(expires_at.as_millis())
        .bind(at.as_millis())
        .fetch_optional(&mut *tx)
        .await?;
        tx.commit().await?;
        match row {
            Some(row) => map_lease(&row),
            None => {
                let current = self.get_lease(tenant, account).await?.ok_or_else(|| {
                    StorageError::NotFound {
                        what: "account lease",
                        id: account.as_str().to_owned(),
                    }
                })?;
                if current.holder != *holder {
                    return Err(StorageError::Conflict {
                        what: "account lease",
                        id: account.as_str().to_owned(),
                        reason: format!(
                            "held by {} (generation {})",
                            current.holder, current.generation
                        ),
                    });
                }
                Err(StorageError::Conflict {
                    what: "account lease",
                    id: account.as_str().to_owned(),
                    reason: format!(
                        "term expired (generation {}); acquire again",
                        current.generation
                    ),
                })
            }
        }
    }

    /// Releases the worker's own lease; `Ok(false)` when it is not the
    /// holder (or the account is unleased).
    pub async fn release_lease(
        &self,
        tenant: &TenantId,
        account: &AccountId,
        holder: &WorkerId,
    ) -> StorageResult<bool> {
        let mut tx = self.begin_tenant(tenant).await?;
        let result = sqlx::query(
            "DELETE FROM account_leases
             WHERE tenant_id = $1 AND account_id = $2 AND holder = $3",
        )
        .bind(tenant.as_str())
        .bind(account.as_str())
        .bind(holder.as_str())
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(result.rows_affected() == 1)
    }

    /// The current lease on `account`, if any.
    pub async fn get_lease(
        &self,
        tenant: &TenantId,
        account: &AccountId,
    ) -> StorageResult<Option<AccountLease>> {
        let mut tx = self.begin_tenant(tenant).await?;
        let row = sqlx::query(
            "SELECT tenant_id, account_id, holder, generation, expires_at, acquired_at
             FROM account_leases WHERE tenant_id = $1 AND account_id = $2",
        )
        .bind(tenant.as_str())
        .bind(account.as_str())
        .fetch_optional(&mut *tx)
        .await?;
        tx.commit().await?;
        match row {
            Some(row) => Ok(Some(map_lease(&row)?)),
            None => Ok(None),
        }
    }
}

fn map_lease(row: &sqlx::postgres::PgRow) -> StorageResult<AccountLease> {
    Ok(AccountLease {
        tenant_id: TenantId::parse(row.try_get::<String, _>("tenant_id")?)?,
        account_id: AccountId::parse(row.try_get::<String, _>("account_id")?)?,
        holder: WorkerId::parse(row.try_get::<String, _>("holder")?)?,
        generation: row.try_get("generation")?,
        expires_at: UnixMillis::from_millis(row.try_get::<i64, _>("expires_at")?)?,
        acquired_at: UnixMillis::from_millis(row.try_get::<i64, _>("acquired_at")?)?,
    })
}
