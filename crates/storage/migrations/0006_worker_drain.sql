-- F013: worker drain state.
--
-- Draining is control-plane state: once an operator asks a worker to
-- stand down, the scheduler must keep refusing to place new accounts on
-- it and migrate its live leases elsewhere. The flag is persisted so a
-- scheduler restart does not forget an in-flight drain, and worker
-- re-registration (which refreshes name/capabilities/heartbeat) must
-- not clear it — only an explicit un-drain does. `drain_requested_at`
-- freezes at the first request so operators can tell how long the
-- drain has been running.

ALTER TABLE workers ADD COLUMN IF NOT EXISTS draining BOOLEAN NOT NULL DEFAULT FALSE;
ALTER TABLE workers ADD COLUMN IF NOT EXISTS drain_requested_at BIGINT;
