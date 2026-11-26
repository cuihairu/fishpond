//! Data retention (F040): the operational sweep that removes terminal
//! rows past their cutoff while the audit event log is never touched.
//!
//! The sweep runs inside a tenant-bound transaction like every other
//! repository method — isolation stays enforced twice (the explicit
//! `WHERE tenant_id = $1` clauses *and* the migrations' row-level
//! security). Live rows and rows inside the retention window are never
//! candidates, and a second sweep with the same cutoff finds nothing
//! left to remove. Multi-tenant deployments iterate tenants (the CLI
//! enumerates them with its operational access; F037 schedules
//! per-tenant).

use fishpond_domain::{TenantId, UnixMillis};
use sqlx::Row;

use crate::error::StorageResult;
use crate::store::Store;

/// What one [`Store::purge_expired`] sweep removed, per table.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PurgeReport {
    /// Terminal authorization sessions (`EXPIRED`/`REVOKED`/`FAILED`)
    /// closed before the cutoff — guarded against live takeover grants.
    pub sessions: u64,
    /// Expired or revoked takeover grants.
    pub grants: u64,
    /// Settled operation attempts of the purged operations.
    pub attempts: u64,
    /// Terminal operations (`SUCCEEDED`/`FAILED`/`CANCELLED`) closed
    /// before the cutoff.
    pub operations: u64,
    /// Terminal webhook deliveries (`delivered`/`abandoned`).
    pub deliveries: u64,
}

/// Terminal auth-session states (wire names, see domain::AuthSessionState).
const TERMINAL_SESSIONS: &str = "('EXPIRED', 'REVOKED', 'FAILED')";
/// Terminal operation states (wire names, see domain::OperationState).
const TERMINAL_OPERATIONS: &str = "('SUCCEEDED', 'FAILED', 'CANCELLED')";

impl Store {
    /// Removes every terminal row of `tenant` closed before `before`
    /// and reports the per-table counts. The event log is the audit
    /// trail and is never swept here — retention for it is a
    /// deliberate, separate decision.
    ///
    /// Sessions still referenced by a live takeover grant (issued,
    /// unexpired, unrevoked) are skipped: the grant's verify path may
    /// still need its session to exist.
    pub async fn purge_expired(
        &self,
        tenant: &TenantId,
        before: UnixMillis,
    ) -> StorageResult<PurgeReport> {
        self.sweep(tenant, before, true).await
    }

    /// [`Self::purge_expired`] without the commit: the sweep runs in a
    /// transaction that is rolled back, so the report is exactly what a
    /// real sweep would remove and nothing changes. The CLI's
    /// `--dry-run` builds on this.
    pub async fn dry_run_purge(
        &self,
        tenant: &TenantId,
        before: UnixMillis,
    ) -> StorageResult<PurgeReport> {
        self.sweep(tenant, before, false).await
    }

    /// Tenant ids visible to this connection's role. An RLS-restricted
    /// role sees none (fail closed) — callers that get an empty list
    /// must pass tenants explicitly instead of assuming the deployment
    /// has none.
    pub async fn visible_tenant_ids(&self) -> StorageResult<Vec<String>> {
        let mut tx = self.pool().acquire().await?;
        let rows = sqlx::query("SELECT id FROM tenants ORDER BY id")
            .fetch_all(&mut *tx)
            .await?;
        let mut ids = Vec::with_capacity(rows.len());
        for row in rows {
            ids.push(row.try_get::<String, _>("id")?);
        }
        Ok(ids)
    }

    /// The shared sweep body; `commit` distinguishes the real sweep
    /// from the dry run's rollback.
    async fn sweep(
        &self,
        tenant: &TenantId,
        before: UnixMillis,
        commit: bool,
    ) -> StorageResult<PurgeReport> {
        let mut tx = self.begin_tenant(tenant).await?;
        let mut report = PurgeReport::default();
        report.grants += sqlx::query(
            "DELETE FROM takeover_grants \
             WHERE tenant_id = $1 AND (revoked_at IS NOT NULL OR expires_at < $2)",
        )
        .bind(tenant.as_str())
        .bind(before.as_millis())
        .execute(&mut *tx)
        .await?
        .rows_affected();
        report.sessions += sqlx::query(&format!(
            "DELETE FROM auth_sessions s \
             WHERE s.tenant_id = $1 AND s.updated_at < $2 \
             AND s.state IN {TERMINAL_SESSIONS} \
             AND NOT EXISTS (SELECT 1 FROM takeover_grants g \
                 WHERE g.tenant_id = s.tenant_id AND g.session_id = s.id)"
        ))
        .bind(tenant.as_str())
        .bind(before.as_millis())
        .execute(&mut *tx)
        .await?
        .rows_affected();
        report.attempts += sqlx::query(&format!(
            "DELETE FROM operation_attempts a \
             WHERE a.tenant_id = $1 AND EXISTS (SELECT 1 FROM operations o \
                 WHERE o.tenant_id = a.tenant_id AND o.id = a.operation_id \
                 AND o.state IN {TERMINAL_OPERATIONS} AND o.updated_at < $2)"
        ))
        .bind(tenant.as_str())
        .bind(before.as_millis())
        .execute(&mut *tx)
        .await?
        .rows_affected();
        report.operations += sqlx::query(&format!(
            "DELETE FROM operations \
             WHERE tenant_id = $1 AND state IN {TERMINAL_OPERATIONS} AND updated_at < $2"
        ))
        .bind(tenant.as_str())
        .bind(before.as_millis())
        .execute(&mut *tx)
        .await?
        .rows_affected();
        report.deliveries += sqlx::query(
            "DELETE FROM webhook_deliveries \
             WHERE tenant_id = $1 AND status IN ('delivered', 'abandoned') \
             AND updated_at < $2",
        )
        .bind(tenant.as_str())
        .bind(before.as_millis())
        .execute(&mut *tx)
        .await?
        .rows_affected();
        if commit {
            tx.commit().await?;
        } else {
            tx.rollback().await?;
        }
        Ok(report)
    }
}
