-- F022: browser profiles, their exclusive locks and lifecycle.
--
-- A profile is a browser context's durable home (cookies, storage) that
-- binds an automation account to a directory on disk. The database
-- holds metadata only: the directory path, a lifecycle state and an
-- optional F008-sealed passphrase — the profile's own contents never
-- enter the database. `state` walks active → backed_up is bookkeeping
-- (backup digests), while destroyed is a tombstone: a destroyed
-- profile keeps its row for audit but must never be locked again.
--
-- A profile lock is the exclusive right to use the profile on disk:
-- the same CAS discipline as account leases (F012) — at most one live
-- holder, generation as the fencing token, expiry as the steal path.
--
-- Both tables are tenant-owned with the same row-level security as the
-- other tenant tables: unbound sessions see and write nothing.

CREATE TABLE IF NOT EXISTS profiles (
    tenant_id          TEXT   NOT NULL REFERENCES tenants (id),
    id                 TEXT   NOT NULL,
    account_id         TEXT   NOT NULL REFERENCES accounts (id),
    label              TEXT   NOT NULL,
    state              TEXT   NOT NULL,
    storage_path       TEXT   NOT NULL,
    secret_nonce       BYTEA  NOT NULL,
    secret_cipher      BYTEA  NOT NULL,
    dek_nonce          BYTEA  NOT NULL,
    wrapped_dek        BYTEA  NOT NULL,
    kek_version        INTEGER NOT NULL,
    secret_fingerprint TEXT   NOT NULL,
    backup_digest      TEXT,
    last_backup_at     BIGINT,
    created_at         BIGINT NOT NULL,
    updated_at         BIGINT NOT NULL,
    PRIMARY KEY (tenant_id, id)
);
CREATE INDEX IF NOT EXISTS idx_profiles_tenant ON profiles (tenant_id);

CREATE TABLE IF NOT EXISTS profile_locks (
    tenant_id   TEXT   NOT NULL REFERENCES tenants (id),
    profile_id  TEXT   NOT NULL,
    holder      TEXT   NOT NULL,
    generation  BIGINT NOT NULL,
    expires_at  BIGINT NOT NULL,
    acquired_at BIGINT NOT NULL,
    PRIMARY KEY (tenant_id, profile_id)
);

DROP POLICY IF EXISTS profiles_tenant_isolation ON profiles;
CREATE POLICY profiles_tenant_isolation ON profiles
    USING (tenant_id = current_setting('app.tenant', true))
    WITH CHECK (tenant_id = current_setting('app.tenant', true));

ALTER TABLE profiles ENABLE ROW LEVEL SECURITY;
ALTER TABLE profiles FORCE ROW LEVEL SECURITY;

DROP POLICY IF EXISTS profile_locks_tenant_isolation ON profile_locks;
CREATE POLICY profile_locks_tenant_isolation ON profile_locks
    USING (tenant_id = current_setting('app.tenant', true))
    WITH CHECK (tenant_id = current_setting('app.tenant', true));

ALTER TABLE profile_locks ENABLE ROW LEVEL SECURITY;
ALTER TABLE profile_locks FORCE ROW LEVEL SECURITY;
