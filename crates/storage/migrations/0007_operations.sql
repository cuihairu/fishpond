-- F014: operation intents, execution attempts, deadlines and idempotency.
--
-- An operation is one durable intent ("send_message to conv X") with a
-- single idempotency key per tenant: the key carries the operation, so
-- a retried create with the same key replays the original operation
-- instead of duplicating work. An operation is claimed exactly once
-- (CAS on state = PENDING -> RUNNING) and executes as a sequence of
-- numbered attempts; each attempt carries a deadline, and an attempt
-- whose deadline passes while still in flight is reaped as TimedOut —
-- retryable in a new attempt, which is how a timed-out attempt differs
-- from a terminal Failed one.
--
-- Both tables are tenant-owned and get the same row-level security as
-- the other tenant tables. `claimed_by` and `attempt.worker` are soft
-- references to workers: a lease or registration row may briefly
-- outlive or precede them.

CREATE TABLE IF NOT EXISTS operations (
    id               TEXT   NOT NULL,
    tenant_id        TEXT   NOT NULL REFERENCES tenants (id),
    kind             TEXT   NOT NULL,
    state            TEXT   NOT NULL,
    idempotency_key  TEXT   NOT NULL,
    claimed_by       TEXT,
    created_at       BIGINT NOT NULL,
    updated_at       BIGINT NOT NULL,
    PRIMARY KEY (tenant_id, id),
    UNIQUE (tenant_id, idempotency_key)
);

CREATE TABLE IF NOT EXISTS operation_attempts (
    id            TEXT   NOT NULL,
    tenant_id     TEXT   NOT NULL REFERENCES tenants (id),
    operation_id  TEXT   NOT NULL,
    FOREIGN KEY (tenant_id, operation_id) REFERENCES operations (tenant_id, id),
    attempt_no    BIGINT NOT NULL,
    worker        TEXT,
    outcome       TEXT,
    started_at    BIGINT NOT NULL,
    deadline_at   BIGINT NOT NULL,
    PRIMARY KEY (tenant_id, id),
    UNIQUE (tenant_id, operation_id, attempt_no)
);

CREATE INDEX IF NOT EXISTS idx_operations_tenant ON operations (tenant_id);
CREATE INDEX IF NOT EXISTS idx_operation_attempts_operation
    ON operation_attempts (tenant_id, operation_id);
-- The deadline reaper scans running operations for in-flight attempts
-- whose deadline has passed.
CREATE INDEX IF NOT EXISTS idx_operation_attempts_deadline
    ON operation_attempts (tenant_id, deadline_at);

DROP POLICY IF EXISTS operations_tenant_isolation ON operations;
CREATE POLICY operations_tenant_isolation ON operations
    USING (tenant_id = current_setting('app.tenant', true))
    WITH CHECK (tenant_id = current_setting('app.tenant', true));

ALTER TABLE operations ENABLE ROW LEVEL SECURITY;
ALTER TABLE operations FORCE ROW LEVEL SECURITY;

DROP POLICY IF EXISTS operation_attempts_tenant_isolation ON operation_attempts;
CREATE POLICY operation_attempts_tenant_isolation ON operation_attempts
    USING (tenant_id = current_setting('app.tenant', true))
    WITH CHECK (tenant_id = current_setting('app.tenant', true));

ALTER TABLE operation_attempts ENABLE ROW LEVEL SECURITY;
ALTER TABLE operation_attempts FORCE ROW LEVEL SECURITY;
