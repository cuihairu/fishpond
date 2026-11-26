-- F027: the operations-plane tables for health registration, capability
-- disabling and incident tracking.
--
-- These are deployment-wide tables, deliberately OUTSIDE the tenant
-- row-level security: the monitor is the operator plane — an unbound
-- (non-tenant) session is exactly who reads platform health and flips
-- capability overrides. Incidents carry a `tenant_id` reference (''
-- means platform-wide) so tenant-attributed incidents stay queryable
-- per tenant at the query level; the column is not a tenancy boundary.
--
-- platform_health: last-write-wins probe result per platform — the
-- provider that served it, an operator-readable status
-- (healthy|degraded|down|unknown) and a free detail note.
--
-- capability_overrides: the kill switch. `disabled = true` refuses new
-- calls for (platform, capability) regardless of scopes or manifest
-- support; the reason and changed_at stamp the operational decision.
--
-- incidents: operator-visible problems, opened idempotently (replays
-- of the same id are no-ops), closed once (a closed incident never
-- re-closes), listed newest-first.

CREATE TABLE IF NOT EXISTS platform_health (
    platform    TEXT   PRIMARY KEY,
    provider_id TEXT   NOT NULL,
    status      TEXT   NOT NULL,
    detail      TEXT   NOT NULL DEFAULT '',
    checked_at  BIGINT NOT NULL
);

CREATE TABLE IF NOT EXISTS capability_overrides (
    platform   TEXT   NOT NULL,
    capability TEXT   NOT NULL,
    disabled   BOOLEAN NOT NULL,
    reason     TEXT   NOT NULL DEFAULT '',
    changed_at BIGINT NOT NULL,
    PRIMARY KEY (platform, capability)
);

CREATE TABLE IF NOT EXISTS incidents (
    tenant_id  TEXT   NOT NULL DEFAULT '',
    id         TEXT   NOT NULL,
    kind       TEXT   NOT NULL,
    severity   TEXT   NOT NULL,
    subject    TEXT   NOT NULL,
    detail     TEXT   NOT NULL DEFAULT '',
    opened_at  BIGINT NOT NULL,
    closed_at  BIGINT,
    PRIMARY KEY (tenant_id, id)
);
CREATE INDEX IF NOT EXISTS idx_incidents_open
    ON incidents (tenant_id, opened_at) WHERE closed_at IS NULL;
