//! F027 monitor repositories: platform health, capability kill
//! switches and the incident log. Deployment-wide rows — these tables
//! sit outside tenant RLS on purpose (the operator plane), so the
//! methods use plain transactions, never [`Store::begin_tenant`].
//! Incidents keep a `tenant_id` reference ('' = platform-wide) that
//! scopes *listing* at the query level; it is not a security boundary.

use sqlx::Row;

use crate::error::StorageResult;
use crate::store::Store;

/// The last probe result recorded for one platform.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HealthRecord {
    /// The platform the probe ran against.
    pub platform: String,
    /// The provider that served the probe.
    pub provider_id: String,
    /// Operator-readable status (`healthy|degraded|down|unknown`).
    pub status: String,
    /// Free detail note (secret-free).
    pub detail: String,
    /// When the probe ran (millis since epoch).
    pub checked_at: i64,
}

/// The kill-switch row for one (platform, capability) pair.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapabilityOverride {
    /// The platform the override applies to.
    pub platform: String,
    /// The capability name (`Capability::as_str`).
    pub capability: String,
    /// True when new calls are refused.
    pub disabled: bool,
    /// Why the switch was flipped (secret-free).
    pub reason: String,
    /// When the switch was flipped (millis since epoch).
    pub changed_at: i64,
}

/// One operator-visible problem.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IncidentRecord {
    /// Owning tenant ('' = platform-wide).
    pub tenant: String,
    /// Caller-chosen id; opens are idempotent per id.
    pub id: String,
    /// The problem class (e.g. "contract-anomaly").
    pub kind: String,
    /// `info|warning|critical`.
    pub severity: String,
    /// What the incident is about (secret-free).
    pub subject: String,
    /// Free detail note (secret-free).
    pub detail: String,
    /// When it opened (millis since epoch).
    pub opened_at: i64,
    /// When it closed, if it did.
    pub closed_at: Option<i64>,
}

/// The input to an idempotent open.
#[derive(Debug, Clone)]
pub struct NewIncident<'a> {
    /// Owning tenant ('' = platform-wide).
    pub tenant: &'a str,
    /// Caller-chosen id; replays are no-ops.
    pub id: &'a str,
    /// The problem class.
    pub kind: &'a str,
    /// `info|warning|critical`.
    pub severity: &'a str,
    /// What the incident is about.
    pub subject: &'a str,
    /// Free detail note.
    pub detail: &'a str,
    /// Open instant (millis since epoch).
    pub opened_at: i64,
}

fn health_row(row: &sqlx::postgres::PgRow) -> HealthRecord {
    HealthRecord {
        platform: row.get("platform"),
        provider_id: row.get("provider_id"),
        status: row.get("status"),
        detail: row.get("detail"),
        checked_at: row.get("checked_at"),
    }
}

fn override_row(row: &sqlx::postgres::PgRow) -> CapabilityOverride {
    CapabilityOverride {
        platform: row.get("platform"),
        capability: row.get("capability"),
        disabled: row.get("disabled"),
        reason: row.get("reason"),
        changed_at: row.get("changed_at"),
    }
}

fn incident_row(row: &sqlx::postgres::PgRow) -> IncidentRecord {
    IncidentRecord {
        tenant: row.get("tenant_id"),
        id: row.get("id"),
        kind: row.get("kind"),
        severity: row.get("severity"),
        subject: row.get("subject"),
        detail: row.get("detail"),
        opened_at: row.get("opened_at"),
        closed_at: row.get("closed_at"),
    }
}

impl Store {
    // ---- platform health ----

    /// Records (last-write-wins) the latest probe result for the
    /// platform's provider.
    pub async fn upsert_health(&self, health: &HealthRecord) -> StorageResult<()> {
        sqlx::query(
            "INSERT INTO platform_health (platform, provider_id, status, detail, checked_at)
             VALUES ($1, $2, $3, $4, $5)
             ON CONFLICT (platform) DO UPDATE SET
                provider_id = EXCLUDED.provider_id,
                status = EXCLUDED.status,
                detail = EXCLUDED.detail,
                checked_at = EXCLUDED.checked_at",
        )
        .bind(&health.platform)
        .bind(&health.provider_id)
        .bind(&health.status)
        .bind(&health.detail)
        .bind(health.checked_at)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Every platform's latest health, ordered by platform.
    pub async fn list_health(&self) -> StorageResult<Vec<HealthRecord>> {
        let rows = sqlx::query(
            "SELECT platform, provider_id, status, detail, checked_at
             FROM platform_health ORDER BY platform",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.iter().map(health_row).collect())
    }

    /// The latest health of one platform, if ever probed.
    pub async fn health_of(&self, platform: &str) -> StorageResult<Option<HealthRecord>> {
        let row = sqlx::query(
            "SELECT platform, provider_id, status, detail, checked_at
             FROM platform_health WHERE platform = $1",
        )
        .bind(platform)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|row| health_row(&row)))
    }

    // ---- capability overrides ----

    /// Flips the kill switch for (platform, capability) — the write is
    /// the full row, so enabling passes `disabled = false`.
    pub async fn set_capability_override(
        &self,
        override_row: &CapabilityOverride,
    ) -> StorageResult<()> {
        sqlx::query(
            "INSERT INTO capability_overrides (platform, capability, disabled, reason, changed_at)
             VALUES ($1, $2, $3, $4, $5)
             ON CONFLICT (platform, capability) DO UPDATE SET
                disabled = EXCLUDED.disabled,
                reason = EXCLUDED.reason,
                changed_at = EXCLUDED.changed_at",
        )
        .bind(&override_row.platform)
        .bind(&override_row.capability)
        .bind(override_row.disabled)
        .bind(&override_row.reason)
        .bind(override_row.changed_at)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Every override, ordered by platform then capability.
    pub async fn list_capability_overrides(&self) -> StorageResult<Vec<CapabilityOverride>> {
        let rows = sqlx::query(
            "SELECT platform, capability, disabled, reason, changed_at
             FROM capability_overrides ORDER BY platform, capability",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.iter().map(override_row).collect())
    }

    /// The override for one pair, if any was ever set. Absent means
    /// "no operator decision" — callers treat that as allowed.
    pub async fn find_capability_override(
        &self,
        platform: &str,
        capability: &str,
    ) -> StorageResult<Option<CapabilityOverride>> {
        let row = sqlx::query(
            "SELECT platform, capability, disabled, reason, changed_at
             FROM capability_overrides WHERE platform = $1 AND capability = $2",
        )
        .bind(platform)
        .bind(capability)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|row| override_row(&row)))
    }

    // ---- incidents ----

    /// Opens an incident; replaying the same (tenant, id) is a no-op.
    /// Returns true when the row is new, false on replay.
    pub async fn open_incident(&self, incident: &NewIncident<'_>) -> StorageResult<bool> {
        let result = sqlx::query(
            "INSERT INTO incidents
                (tenant_id, id, kind, severity, subject, detail, opened_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7)
             ON CONFLICT (tenant_id, id) DO NOTHING",
        )
        .bind(incident.tenant)
        .bind(incident.id)
        .bind(incident.kind)
        .bind(incident.severity)
        .bind(incident.subject)
        .bind(incident.detail)
        .bind(incident.opened_at)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() == 1)
    }

    /// One incident, if it exists in the tenant's scope.
    pub async fn incident(&self, tenant: &str, id: &str) -> StorageResult<Option<IncidentRecord>> {
        let row = sqlx::query(
            "SELECT tenant_id, id, kind, severity, subject, detail, opened_at, closed_at
             FROM incidents WHERE tenant_id = $1 AND id = $2",
        )
        .bind(tenant)
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|row| incident_row(&row)))
    }

    /// Closes an open incident. Returns false when the incident is
    /// absent (or already closed) — a closed incident never re-closes.
    pub async fn close_incident(
        &self,
        tenant: &str,
        id: &str,
        closed_at: i64,
    ) -> StorageResult<bool> {
        let result = sqlx::query(
            "UPDATE incidents SET closed_at = $3
             WHERE tenant_id = $1 AND id = $2 AND closed_at IS NULL",
        )
        .bind(tenant)
        .bind(id)
        .bind(closed_at)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() == 1)
    }

    /// The tenant's incidents, newest first; `open_only` filters to
    /// the ones not yet closed. Listing never crosses the tenant
    /// reference.
    pub async fn list_incidents(
        &self,
        tenant: &str,
        open_only: bool,
    ) -> StorageResult<Vec<IncidentRecord>> {
        let rows = sqlx::query(
            "SELECT tenant_id, id, kind, severity, subject, detail, opened_at, closed_at
             FROM incidents
             WHERE tenant_id = $1 AND ($2 = FALSE OR closed_at IS NULL)
             ORDER BY opened_at DESC, id DESC",
        )
        .bind(tenant)
        .bind(open_only)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.iter().map(incident_row).collect())
    }
}
