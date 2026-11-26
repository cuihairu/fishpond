-- F017: the tenant event stream — durable log (the outbox write side),
-- consumer inbox (dedup) and projection checkpoints (cursors).
--
-- `events` is the committed log: one row per event, sequenced per
-- tenant with no gaps in the log itself (append serializes on the
-- tenant row and stamps MAX(seq)+1, so concurrent appends cannot race
-- a hole into the sequence). Consumers that receive events through a
-- lossy transport (webhooks, SSE) may still observe gaps between their
-- checkpoint and the log — the log is the truth they replay against.
--
-- `event_inbox` is the consumer-side dedup: at-least-once delivery
-- meets at-most-once apply. A claim INSERT ... ON CONFLICT DO NOTHING
-- lets exactly one of N redeliveries apply the event; `release` gives
-- the row back when the apply failed, so retries stay possible.
--
-- `projection_checkpoints` is each consumer's cursor into the log:
-- "everything up to last_seq is applied", which is also the baseline
-- the gap detector compares a delivered batch against.
--
-- All three are tenant-owned with the same row-level security as the
-- rest of the schema.

CREATE TABLE IF NOT EXISTS events (
    id          TEXT   NOT NULL,
    tenant_id   TEXT   NOT NULL REFERENCES tenants (id),
    seq         BIGINT NOT NULL,
    event_type  TEXT   NOT NULL,
    account_id  TEXT,
    occurred_at BIGINT NOT NULL,
    payload     TEXT   NOT NULL,
    PRIMARY KEY (tenant_id, id),
    UNIQUE (tenant_id, seq)
);

CREATE INDEX IF NOT EXISTS idx_events_seq ON events (tenant_id, seq);

CREATE TABLE IF NOT EXISTS event_inbox (
    tenant_id    TEXT   NOT NULL REFERENCES tenants (id),
    consumer     TEXT   NOT NULL,
    event_id     TEXT   NOT NULL,
    seq          BIGINT NOT NULL,
    claimed_at   BIGINT NOT NULL,
    PRIMARY KEY (tenant_id, consumer, event_id)
);

CREATE INDEX IF NOT EXISTS idx_event_inbox_consumer
    ON event_inbox (tenant_id, consumer, seq);

CREATE TABLE IF NOT EXISTS projection_checkpoints (
    tenant_id  TEXT   NOT NULL REFERENCES tenants (id),
    consumer   TEXT   NOT NULL,
    last_seq   BIGINT NOT NULL,
    updated_at BIGINT NOT NULL,
    PRIMARY KEY (tenant_id, consumer)
);

DROP POLICY IF EXISTS events_tenant_isolation ON events;
CREATE POLICY events_tenant_isolation ON events
    USING (tenant_id = current_setting('app.tenant', true))
    WITH CHECK (tenant_id = current_setting('app.tenant', true));

ALTER TABLE events ENABLE ROW LEVEL SECURITY;
ALTER TABLE events FORCE ROW LEVEL SECURITY;

DROP POLICY IF EXISTS event_inbox_tenant_isolation ON event_inbox;
CREATE POLICY event_inbox_tenant_isolation ON event_inbox
    USING (tenant_id = current_setting('app.tenant', true))
    WITH CHECK (tenant_id = current_setting('app.tenant', true));

ALTER TABLE event_inbox ENABLE ROW LEVEL SECURITY;
ALTER TABLE event_inbox FORCE ROW LEVEL SECURITY;

DROP POLICY IF EXISTS projection_checkpoints_tenant_isolation ON projection_checkpoints;
CREATE POLICY projection_checkpoints_tenant_isolation ON projection_checkpoints
    USING (tenant_id = current_setting('app.tenant', true))
    WITH CHECK (tenant_id = current_setting('app.tenant', true));

ALTER TABLE projection_checkpoints ENABLE ROW LEVEL SECURITY;
ALTER TABLE projection_checkpoints FORCE ROW LEVEL SECURITY;
