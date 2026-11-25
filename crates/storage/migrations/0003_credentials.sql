-- F008: credential storage behind envelope encryption.
--
-- Only ciphertext reaches this database: the secret is encrypted with a
-- per-credential DEK, and the DEK is wrapped under the master key whose
-- generation is recorded in `kek_version`. Master-key material itself is
-- never stored here — it is mounted from configuration
-- (`MasterKey::from_env`), which is what makes rotation and revocation
-- possible without the ciphertext ever changing.
--
-- `credentials` is tenant-owned, so it gets the same row-level security as
-- the F007 tables: unbound sessions cannot read or write another tenant's
-- credentials. `master_keys` is global (key generations are not tenant
-- data) and carries no RLS, like `api_keys` and `tenant_roles`.

CREATE TABLE IF NOT EXISTS master_keys (
    version    INTEGER PRIMARY KEY,
    created_at BIGINT NOT NULL,
    is_retired BOOLEAN NOT NULL DEFAULT FALSE
);

CREATE TABLE IF NOT EXISTS credentials (
    id             TEXT   PRIMARY KEY,
    tenant_id      TEXT   NOT NULL REFERENCES tenants (id),
    platform       TEXT   NOT NULL,
    kind           TEXT   NOT NULL,
    secret_nonce   BYTEA  NOT NULL,
    secret_cipher  BYTEA  NOT NULL,
    dek_nonce      BYTEA  NOT NULL,
    wrapped_dek    BYTEA  NOT NULL,
    kek_version    INTEGER NOT NULL,
    fingerprint    TEXT,
    created_at     BIGINT NOT NULL,
    is_revoked     BOOLEAN NOT NULL DEFAULT FALSE,
    revoked_at     BIGINT
);
CREATE INDEX IF NOT EXISTS idx_credentials_tenant ON credentials (tenant_id);
CREATE INDEX IF NOT EXISTS idx_credentials_platform ON credentials (platform, tenant_id);

DROP POLICY IF EXISTS credentials_tenant_isolation ON credentials;
CREATE POLICY credentials_tenant_isolation ON credentials
    USING (tenant_id = current_setting('app.tenant', true))
    WITH CHECK (tenant_id = current_setting('app.tenant', true));

ALTER TABLE credentials ENABLE ROW LEVEL SECURITY;
ALTER TABLE credentials FORCE ROW LEVEL SECURITY;
