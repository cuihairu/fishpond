-- F018: webhook subscriptions and their delivery queue.
--
-- `webhook_subscriptions` is the tenant-owned endpoint registry. The
-- signing secret follows the F008 envelope scheme (per-subscription
-- DEK wrapped under a master-key generation): only ciphertext reaches
-- this database, and the fingerprint column is for correlation, never
-- the secret. `event_type` filters which events fan out (NULL = all).
--
-- `webhook_deliveries` is the per-subscription delivery queue fanned
-- out from the F017 event log. Each row snapshots the exact JSON body
-- it will POST, so retries sign the same bytes and redelivery is
-- byte-stable. Delivery is at-least-once: `pending` rows whose
-- `next_attempt_at` is due are claimed to `delivering`, and stale
-- `delivering` rows (crashed loop) are reclaimed. `last_error` holds
-- transport outcome text only — never secret material.
--
-- Both tables are tenant-owned with the same row-level security as the
-- rest of the schema.

CREATE TABLE IF NOT EXISTS webhook_subscriptions (
    id            TEXT   NOT NULL,
    tenant_id     TEXT   NOT NULL REFERENCES tenants (id),
    url           TEXT   NOT NULL,
    secret_nonce  BYTEA  NOT NULL,
    secret_cipher BYTEA  NOT NULL,
    dek_nonce     BYTEA  NOT NULL,
    wrapped_dek   BYTEA  NOT NULL,
    kek_version   INTEGER NOT NULL,
    fingerprint   TEXT,
    event_type    TEXT,
    is_enabled    BOOLEAN NOT NULL DEFAULT TRUE,
    created_at    BIGINT NOT NULL,
    updated_at    BIGINT NOT NULL,
    PRIMARY KEY (tenant_id, id)
);

CREATE TABLE IF NOT EXISTS webhook_deliveries (
    id              TEXT   NOT NULL,
    tenant_id       TEXT   NOT NULL REFERENCES tenants (id),
    subscription_id TEXT   NOT NULL,
    event_id        TEXT   NOT NULL,
    event_seq       BIGINT NOT NULL,
    body            TEXT   NOT NULL,
    attempts        INTEGER NOT NULL DEFAULT 0,
    status          TEXT   NOT NULL DEFAULT 'pending',
    next_attempt_at BIGINT NOT NULL,
    last_error      TEXT,
    created_at      BIGINT NOT NULL,
    updated_at      BIGINT NOT NULL,
    PRIMARY KEY (tenant_id, id)
);

CREATE INDEX IF NOT EXISTS idx_webhook_deliveries_due
    ON webhook_deliveries (tenant_id, status, next_attempt_at);

DROP POLICY IF EXISTS webhook_subscriptions_tenant_isolation ON webhook_subscriptions;
CREATE POLICY webhook_subscriptions_tenant_isolation ON webhook_subscriptions
    USING (tenant_id = current_setting('app.tenant', true))
    WITH CHECK (tenant_id = current_setting('app.tenant', true));

ALTER TABLE webhook_subscriptions ENABLE ROW LEVEL SECURITY;
ALTER TABLE webhook_subscriptions FORCE ROW LEVEL SECURITY;

DROP POLICY IF EXISTS webhook_deliveries_tenant_isolation ON webhook_deliveries;
CREATE POLICY webhook_deliveries_tenant_isolation ON webhook_deliveries
    USING (tenant_id = current_setting('app.tenant', true))
    WITH CHECK (tenant_id = current_setting('app.tenant', true));

ALTER TABLE webhook_deliveries ENABLE ROW LEVEL SECURITY;
ALTER TABLE webhook_deliveries FORCE ROW LEVEL SECURITY;
