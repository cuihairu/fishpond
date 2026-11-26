-- F015: the account observation audit trail behind reconcile and manual
-- verification.
--
-- The accounts row carries only the *current* confirmed state, and by
-- domain invariant a confirmed state carries no note — so "who resolved
-- the UNKNOWN, and how" would be unrecorded. Every reconcile pass and
-- every operator confirmation appends one row here: `source` splits
-- automatic probes (`reconcile`) from human decisions (`manual`),
-- `operator` names the human (NULL for probes), and `note` carries the
-- probe reason or the operator's justification. The accounts row stays
-- the live projection; this table is the audit history.
--
-- Tenant-owned with the same row-level security as the rest of the
-- schema; account is a soft reference (an account row may be deleted
-- without rewriting history).

CREATE TABLE IF NOT EXISTS account_observations (
    id          TEXT   NOT NULL,
    tenant_id   TEXT   NOT NULL REFERENCES tenants (id),
    account_id  TEXT   NOT NULL REFERENCES accounts (id),
    source      TEXT   NOT NULL,
    observed    TEXT   NOT NULL,
    outcome     TEXT   NOT NULL,
    note        TEXT,
    operator    TEXT,
    at          BIGINT NOT NULL,
    PRIMARY KEY (tenant_id, id)
);

CREATE INDEX IF NOT EXISTS idx_account_observations_account
    ON account_observations (tenant_id, account_id);

DROP POLICY IF EXISTS account_observations_tenant_isolation ON account_observations;
CREATE POLICY account_observations_tenant_isolation ON account_observations
    USING (tenant_id = current_setting('app.tenant', true))
    WITH CHECK (tenant_id = current_setting('app.tenant', true));

ALTER TABLE account_observations ENABLE ROW LEVEL SECURITY;
ALTER TABLE account_observations FORCE ROW LEVEL SECURITY;
