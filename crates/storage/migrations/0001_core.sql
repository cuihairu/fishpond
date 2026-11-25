-- F006 core schema: tenancy and platform data.
-- Timestamps are unix epoch milliseconds (BIGINT); ids are TEXT because
-- the domain validates id charset/length before anything reaches SQL.

CREATE TABLE IF NOT EXISTS tenants (
    id         TEXT PRIMARY KEY,
    name       TEXT   NOT NULL,
    created_at BIGINT NOT NULL
);

CREATE TABLE IF NOT EXISTS accounts (
    id         TEXT PRIMARY KEY,
    tenant_id  TEXT   NOT NULL REFERENCES tenants (id),
    platform   TEXT   NOT NULL,
    state      TEXT   NOT NULL,
    state_note TEXT,
    updated_at BIGINT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_accounts_tenant ON accounts (tenant_id);

CREATE TABLE IF NOT EXISTS conversations (
    id         TEXT PRIMARY KEY,
    account_id TEXT   NOT NULL REFERENCES accounts (id),
    tenant_id  TEXT   NOT NULL REFERENCES tenants (id),
    updated_at BIGINT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_conversations_tenant_account
    ON conversations (tenant_id, account_id);

CREATE TABLE IF NOT EXISTS messages (
    id              TEXT PRIMARY KEY,
    conversation_id TEXT   NOT NULL REFERENCES conversations (id),
    tenant_id       TEXT   NOT NULL REFERENCES tenants (id),
    direction       TEXT   NOT NULL,
    text            TEXT   NOT NULL,
    sent_at         BIGINT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_messages_conversation_time
    ON messages (tenant_id, conversation_id, sent_at);

CREATE TABLE IF NOT EXISTS products (
    id          TEXT PRIMARY KEY,
    tenant_id   TEXT   NOT NULL REFERENCES tenants (id),
    title       TEXT   NOT NULL,
    price_cents BIGINT NOT NULL,
    currency    TEXT   NOT NULL,
    status      TEXT   NOT NULL,
    updated_at  BIGINT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_products_tenant ON products (tenant_id);
