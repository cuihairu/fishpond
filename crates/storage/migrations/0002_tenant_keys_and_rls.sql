-- F007: API keys with an RBAC role + frozen scope list, and row-level
-- security isolating every tenant-owned table.
--
-- RLS design: `Store::begin_tenant` binds `app.tenant` per transaction
-- (`set_config('app.tenant', <tenant>, TRUE)`); the binding is
-- transaction-local, so it can never leak onto another pool caller.
-- Policies compare each row's tenant against that binding and `FORCE ROW
-- LEVEL SECURITY` makes them apply to the table owner too — the
-- application role is never exempt. Unbound sessions see no rows and may
-- write none: fail closed. Operational/migration access runs as a
-- superuser (superusers bypass RLS by design).
--
-- The auth-time tables (`tenant_roles`, `api_keys`) carry no RLS: a key
-- must be resolvable *before* its tenant is known, and `api_keys` holds a
-- SHA-256 hash plus public metadata only — raw key material never reaches
-- this database.

CREATE TABLE IF NOT EXISTS tenant_roles (
    name   TEXT PRIMARY KEY,
    scopes TEXT[] NOT NULL
);

-- RBAC seed: a role is a named scope bundle; keys freeze their role's
-- scopes at issue time (see `Store::issue_api_key`), so role edits never
-- retroactively change live keys.
INSERT INTO tenant_roles (name, scopes) VALUES
    ('owner',    ARRAY['accounts:read', 'conversations:read', 'conversations:write', 'catalog:read', 'operations:read', 'events:read']),
    ('operator', ARRAY['accounts:read', 'conversations:read', 'conversations:write', 'catalog:read', 'operations:read', 'events:read']),
    ('viewer',   ARRAY['accounts:read', 'conversations:read', 'catalog:read', 'operations:read', 'events:read'])
ON CONFLICT (name) DO NOTHING;

CREATE TABLE IF NOT EXISTS api_keys (
    key_hash   TEXT PRIMARY KEY,
    key_hint   TEXT NOT NULL,
    tenant_id  TEXT NOT NULL REFERENCES tenants (id),
    role       TEXT NOT NULL REFERENCES tenant_roles (name),
    scopes     TEXT[] NOT NULL,
    created_at BIGINT NOT NULL,
    expires_at BIGINT NOT NULL,
    is_revoked BOOLEAN NOT NULL DEFAULT FALSE,
    revoked_at BIGINT
);
CREATE INDEX IF NOT EXISTS idx_api_keys_tenant ON api_keys (tenant_id);

DROP POLICY IF EXISTS tenants_tenant_isolation ON tenants;
DROP POLICY IF EXISTS accounts_tenant_isolation ON accounts;
DROP POLICY IF EXISTS conversations_tenant_isolation ON conversations;
DROP POLICY IF EXISTS messages_tenant_isolation ON messages;
DROP POLICY IF EXISTS products_tenant_isolation ON products;

CREATE POLICY tenants_tenant_isolation ON tenants
    USING (id = current_setting('app.tenant', true))
    WITH CHECK (id = current_setting('app.tenant', true));

CREATE POLICY accounts_tenant_isolation ON accounts
    USING (tenant_id = current_setting('app.tenant', true))
    WITH CHECK (tenant_id = current_setting('app.tenant', true));

CREATE POLICY conversations_tenant_isolation ON conversations
    USING (tenant_id = current_setting('app.tenant', true))
    WITH CHECK (tenant_id = current_setting('app.tenant', true));

CREATE POLICY messages_tenant_isolation ON messages
    USING (tenant_id = current_setting('app.tenant', true))
    WITH CHECK (tenant_id = current_setting('app.tenant', true));

CREATE POLICY products_tenant_isolation ON products
    USING (tenant_id = current_setting('app.tenant', true))
    WITH CHECK (tenant_id = current_setting('app.tenant', true));

-- ENABLE turns the policies on; FORCE makes them apply to the table
-- owner too (the application role must never bypass its own isolation).
ALTER TABLE tenants       ENABLE ROW LEVEL SECURITY;
ALTER TABLE accounts      ENABLE ROW LEVEL SECURITY;
ALTER TABLE conversations ENABLE ROW LEVEL SECURITY;
ALTER TABLE messages      ENABLE ROW LEVEL SECURITY;
ALTER TABLE products      ENABLE ROW LEVEL SECURITY;

ALTER TABLE tenants       FORCE ROW LEVEL SECURITY;
ALTER TABLE accounts      FORCE ROW LEVEL SECURITY;
ALTER TABLE conversations FORCE ROW LEVEL SECURITY;
ALTER TABLE messages      FORCE ROW LEVEL SECURITY;
ALTER TABLE products      FORCE ROW LEVEL SECURITY;
