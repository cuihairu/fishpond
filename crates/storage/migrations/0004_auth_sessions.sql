-- F009: authorization sessions (account login flows).
--
-- One row per login attempt of an account on its platform. The row tracks
-- the session state machine (PENDING → AWAITING_CONFIRMATION → AUTHORIZED
-- → EXPIRED/REVOKED; see domain::AuthSessionState) and, once authorized,
-- which credential carries the login and which master-key generation
-- encrypted it — so a later credential rotation can be detected (F011
-- binds the coordinator to that check).
--
-- Sessions are tenant-owned and get the same row-level security as the
-- other tenant tables: unbound sessions see and write nothing.

CREATE TABLE IF NOT EXISTS auth_sessions (
    id                 TEXT   PRIMARY KEY,
    tenant_id          TEXT   NOT NULL REFERENCES tenants (id),
    account_id         TEXT   NOT NULL REFERENCES accounts (id),
    platform           TEXT   NOT NULL,
    state              TEXT   NOT NULL,
    state_note         TEXT,
    credential_id      TEXT REFERENCES credentials (id),
    credential_version INTEGER,
    created_at         BIGINT NOT NULL,
    updated_at         BIGINT NOT NULL,
    expires_at         BIGINT
);
CREATE INDEX IF NOT EXISTS idx_auth_sessions_tenant ON auth_sessions (tenant_id);
CREATE INDEX IF NOT EXISTS idx_auth_sessions_account ON auth_sessions (tenant_id, account_id);

DROP POLICY IF EXISTS auth_sessions_tenant_isolation ON auth_sessions;
CREATE POLICY auth_sessions_tenant_isolation ON auth_sessions
    USING (tenant_id = current_setting('app.tenant', true))
    WITH CHECK (tenant_id = current_setting('app.tenant', true));

ALTER TABLE auth_sessions ENABLE ROW LEVEL SECURITY;
ALTER TABLE auth_sessions FORCE ROW LEVEL SECURITY;
