-- F012: worker registry and account leases.
--
-- Workers register once and heartbeat while alive; a stale heartbeat
-- marks them lost (detection helper for the scheduler, F013). A lease is
-- the per-account exclusive right to run automation: CAS semantics keep
-- at most one live holder, and `generation` is the fencing token — it
-- increments only when the holder changes (steal), never on renewal, so
-- a worker paused past its expiry can detect that it lost the account.
--
-- Both tables are tenant-owned and get the same row-level security as
-- the other tenant tables: unbound sessions see and write nothing.
-- `holder` is a soft reference (no FK): a lease may briefly outlive its
-- worker row, and cleaning both up is the scheduler's job (F013).

CREATE TABLE IF NOT EXISTS workers (
    id                TEXT   PRIMARY KEY,
    tenant_id         TEXT   NOT NULL REFERENCES tenants (id),
    name              TEXT   NOT NULL,
    capabilities      TEXT[] NOT NULL DEFAULT '{}',
    registered_at     BIGINT NOT NULL,
    last_heartbeat_at BIGINT NOT NULL,
    updated_at        BIGINT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_workers_tenant ON workers (tenant_id);

CREATE TABLE IF NOT EXISTS account_leases (
    tenant_id   TEXT   NOT NULL REFERENCES tenants (id),
    account_id  TEXT   NOT NULL REFERENCES accounts (id),
    holder      TEXT   NOT NULL,
    generation  BIGINT NOT NULL,
    expires_at  BIGINT NOT NULL,
    acquired_at BIGINT NOT NULL,
    PRIMARY KEY (tenant_id, account_id)
);

DROP POLICY IF EXISTS workers_tenant_isolation ON workers;
CREATE POLICY workers_tenant_isolation ON workers
    USING (tenant_id = current_setting('app.tenant', true))
    WITH CHECK (tenant_id = current_setting('app.tenant', true));

ALTER TABLE workers ENABLE ROW LEVEL SECURITY;
ALTER TABLE workers FORCE ROW LEVEL SECURITY;

DROP POLICY IF EXISTS account_leases_tenant_isolation ON account_leases;
CREATE POLICY account_leases_tenant_isolation ON account_leases
    USING (tenant_id = current_setting('app.tenant', true))
    WITH CHECK (tenant_id = current_setting('app.tenant', true));

ALTER TABLE account_leases ENABLE ROW LEVEL SECURITY;
ALTER TABLE account_leases FORCE ROW LEVEL SECURITY;
