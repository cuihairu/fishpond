//! Capacity report (F039): the operator's demand-vs-supply picture of
//! one tenant in a single tenant-bound round trip — accounts to serve,
//! live leases and queued/running operations (demand) against workers
//! and their draining flag (supply), plus the storage-relevant volumes
//! (event log, profiles, sessions, webhook subscriptions).
//!
//! Like every repository method the read runs inside
//! [`Store::begin_tenant`]: row-level security and the explicit
//! `WHERE tenant_id` clauses agree, and another tenant's rows are
//! invisible even though this is a cross-table aggregate.

use fishpond_domain::{TenantId, UnixMillis};
use sqlx::Row;

use crate::error::StorageResult;
use crate::store::Store;

/// One tenant's capacity picture. Terminal operation states share one
/// counter (`SUCCEEDED`/`FAILED`/`CANCELLED`); live leases are those
/// unexpired at the report instant.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CapacityReport {
    /// Accounts registered for the tenant.
    pub accounts_total: u64,
    /// Accounts in the `ONLINE` state.
    pub accounts_online: u64,
    /// Workers registered under the tenant.
    pub workers_total: u64,
    /// Workers marked draining (about to shed their work).
    pub workers_draining: u64,
    /// Leases unexpired at the report instant (the work actually placed).
    pub live_leases: u64,
    /// Operations waiting to be claimed.
    pub operations_pending: u64,
    /// Operations with a running attempt.
    pub operations_running: u64,
    /// Settled operations (`SUCCEEDED`/`FAILED`/`CANCELLED`).
    pub operations_terminal: u64,
    /// Audit event log rows.
    pub events_total: u64,
    /// Profiles in the `active` state.
    pub profiles_active: u64,
    /// Auth sessions not in a terminal state.
    pub sessions_active: u64,
    /// Webhook subscriptions.
    pub webhook_subscriptions: u64,
}

impl Store {
    /// Counts the tenant's capacity picture as of `at` in one query —
    /// every table filtered to the tenant, leases judged against `at`.
    pub async fn tenant_capacity(
        &self,
        tenant: &TenantId,
        at: UnixMillis,
    ) -> StorageResult<CapacityReport> {
        let mut tx = self.begin_tenant(tenant).await?;
        let row = sqlx::query(
            "SELECT \
             (SELECT COUNT(*) FROM accounts WHERE tenant_id = $1) AS accounts_total, \
             (SELECT COUNT(*) FROM accounts WHERE tenant_id = $1 AND state = 'ONLINE') \
                 AS accounts_online, \
             (SELECT COUNT(*) FROM workers WHERE tenant_id = $1) AS workers_total, \
             (SELECT COUNT(*) FROM workers WHERE tenant_id = $1 AND draining) \
                 AS workers_draining, \
             (SELECT COUNT(*) FROM account_leases \
                 WHERE tenant_id = $1 AND expires_at > $2) AS live_leases, \
             (SELECT COUNT(*) FROM operations WHERE tenant_id = $1 AND state = 'PENDING') \
                 AS operations_pending, \
             (SELECT COUNT(*) FROM operations WHERE tenant_id = $1 AND state = 'RUNNING') \
                 AS operations_running, \
             (SELECT COUNT(*) FROM operations \
                 WHERE tenant_id = $1 \
                   AND state IN ('SUCCEEDED', 'FAILED', 'CANCELLED')) \
                 AS operations_terminal, \
             (SELECT COUNT(*) FROM events WHERE tenant_id = $1) AS events_total, \
             (SELECT COUNT(*) FROM profiles WHERE tenant_id = $1 AND state = 'active') \
                 AS profiles_active, \
             (SELECT COUNT(*) FROM auth_sessions \
                 WHERE tenant_id = $1 \
                   AND state NOT IN ('EXPIRED', 'REVOKED', 'FAILED')) \
                 AS sessions_active, \
             (SELECT COUNT(*) FROM webhook_subscriptions WHERE tenant_id = $1) \
                 AS webhook_subscriptions",
        )
        .bind(tenant.as_str())
        .bind(at.as_millis())
        .fetch_one(&mut *tx)
        .await?;
        let report = CapacityReport {
            accounts_total: row.try_get::<i64, _>("accounts_total")? as u64,
            accounts_online: row.try_get::<i64, _>("accounts_online")? as u64,
            workers_total: row.try_get::<i64, _>("workers_total")? as u64,
            workers_draining: row.try_get::<i64, _>("workers_draining")? as u64,
            live_leases: row.try_get::<i64, _>("live_leases")? as u64,
            operations_pending: row.try_get::<i64, _>("operations_pending")? as u64,
            operations_running: row.try_get::<i64, _>("operations_running")? as u64,
            operations_terminal: row.try_get::<i64, _>("operations_terminal")? as u64,
            events_total: row.try_get::<i64, _>("events_total")? as u64,
            profiles_active: row.try_get::<i64, _>("profiles_active")? as u64,
            sessions_active: row.try_get::<i64, _>("sessions_active")? as u64,
            webhook_subscriptions: row.try_get::<i64, _>("webhook_subscriptions")? as u64,
        };
        tx.commit().await?;
        Ok(report)
    }
}
