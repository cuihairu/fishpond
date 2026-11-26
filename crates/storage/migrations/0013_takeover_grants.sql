-- F034: short-lived operator takeover grants (T10, manual takeover).
--
-- A grant is the credential an operator presents to drive a
-- taken-over session remotely: issued once (the raw token exists only
-- in the API response), verified by presenting the token, revoked on
-- release or expiry. Revoked and expired rows stay for audit.
--
-- Like `api_keys`, this is an auth-time table with no row-level
-- security: verification must resolve the grant *before* any tenant
-- is known, and the rows hold no secret material — only the SHA-256
-- token hash and a display tail. Tenant isolation for the
-- administration surface (list/revoke/live lookup) is enforced by
-- explicit tenant_id predicates in the queries.

CREATE TABLE IF NOT EXISTS takeover_grants (
    id         TEXT   PRIMARY KEY,
    tenant_id  TEXT   NOT NULL REFERENCES tenants (id),
    account_id TEXT   NOT NULL REFERENCES accounts (id),
    session_id TEXT   NOT NULL REFERENCES auth_sessions (id),
    operator   TEXT   NOT NULL,
    token_hash TEXT   NOT NULL UNIQUE,
    token_hint TEXT   NOT NULL,
    created_at BIGINT NOT NULL,
    expires_at BIGINT NOT NULL,
    revoked_at BIGINT,
    CONSTRAINT takeover_grants_lifetime CHECK (expires_at > created_at)
);

CREATE INDEX IF NOT EXISTS idx_takeover_grants_tenant ON takeover_grants (tenant_id);
CREATE INDEX IF NOT EXISTS idx_takeover_grants_session ON takeover_grants (tenant_id, session_id);

-- One live grant per session: a taken-over session has exactly one
-- operator credential at a time; release revokes before a new grant
-- can be issued.
CREATE UNIQUE INDEX IF NOT EXISTS uq_takeover_grants_live_session
    ON takeover_grants (session_id) WHERE revoked_at IS NULL;
