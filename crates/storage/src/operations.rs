//! Operation and attempt persistence (F014).
//!
//! Two invariants are enforced in SQL rather than in application
//! memory, so concurrent workers cannot race around them:
//!
//! * **Idempotency** — `(tenant_id, idempotency_key)` is unique. An
//!   insert that hits the key returns the *original* operation (with a
//!   `replayed` flag); it never duplicates work.
//! * **Single claim** — claiming runs as `UPDATE ... WHERE state =
//!   'PENDING' AND ... RETURNING`, so exactly one of concurrent
//!   claimants sees success. A stale claimant gets a
//!   [`StorageError::Conflict`] naming who won.
//!
//! Attempts are numbered from 1 and unique per operation; a timed-out
//! attempt is recorded as such and the operation stays RUNNING for a
//! later attempt, while a terminal failure moves it to FAILED. Deadline
//! reaping is a read of in-flight attempts whose `deadline_at` has
//! passed, plus the CAS that closes them out.
//!
//! Every surface is tenant-bound via [`Store::begin_tenant`], with
//! row-level security as a second isolation layer.

use fishpond_domain::{
    AttemptId, AttemptOutcome, Operation, OperationId, OperationState, TenantId, UnixMillis,
    WorkerId,
};
use sqlx::Row;

use crate::error::{StorageError, StorageResult};
use crate::store::Store;

/// An operation to create.
pub struct NewOperation<'a> {
    /// Operation id.
    pub id: &'a OperationId,
    /// Owning tenant.
    pub tenant: &'a TenantId,
    /// What to do (e.g. `send_message`).
    pub kind: &'a str,
    /// Caller-supplied idempotency key, unique within the tenant.
    pub idempotency_key: &'a str,
    /// Creation instant.
    pub at: UnixMillis,
}

/// Result of a create: the stored operation (original or replayed).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreatedOperation {
    /// The operation row.
    pub operation: Operation,
    /// `true` when the idempotency key already existed and the original
    /// operation was returned (no new work was created).
    pub replayed: bool,
}

/// A stored attempt with its outcome still optional — `None` while the
/// attempt is in flight (started, not yet closed).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttemptRecord {
    /// Attempt id.
    pub attempt_id: AttemptId,
    /// The owning operation.
    pub operation_id: OperationId,
    /// 1-based attempt number.
    pub attempt_no: u32,
    /// The worker that started it (soft reference).
    pub worker: Option<WorkerId>,
    /// The outcome, `None` while still in flight.
    pub outcome: Option<AttemptOutcome>,
    /// When the attempt started.
    pub started_at: UnixMillis,
    /// When the attempt deadline expires.
    pub deadline_at: UnixMillis,
}

/// A claimed operation together with the attempt that represents the
/// claim. The attempt's deadline is when the claim expires.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaimedOperation {
    /// The operation, now RUNNING.
    pub operation: Operation,
    /// The new attempt (1-based number, outcome `None` while running).
    pub attempt: AttemptRecord,
}

/// What to write when an attempt closes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttemptClose {
    /// The operation succeeded; terminal.
    Succeeded,
    /// The attempt failed terminally; the operation fails with it.
    Failed,
    /// The attempt timed out; the operation returns to PENDING for a
    /// later attempt.
    TimedOut,
}

impl AttemptClose {
    /// The attempt outcome this close records.
    pub fn outcome(self) -> AttemptOutcome {
        match self {
            Self::Succeeded => AttemptOutcome::Succeeded,
            Self::Failed => AttemptOutcome::Failed,
            Self::TimedOut => AttemptOutcome::TimedOut,
        }
    }
}

/// An in-flight attempt whose deadline has passed, ready to reap.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExpiredAttempt {
    /// The attempt row (outcome `None`).
    pub attempt: AttemptRecord,
}

impl Store {
    /// Creates an operation, or replays the original when the tenant's
    /// idempotency key already exists. A *different* operation id with
    /// the same key returns a conflict naming the existing id: keys
    /// must not silently alias distinct intents.
    pub async fn create_operation(
        &self,
        new: &NewOperation<'_>,
    ) -> StorageResult<CreatedOperation> {
        if new.idempotency_key.is_empty() {
            return Err(StorageError::Invalid {
                what: "operation idempotency key",
                reason: "must not be empty".to_owned(),
            });
        }
        let mut tx = self.begin_tenant(new.tenant).await?;
        let row = sqlx::query(
            "INSERT INTO operations
                 (id, tenant_id, kind, state, idempotency_key, claimed_by, created_at, updated_at)
             VALUES ($1, $2, $3, 'PENDING', $4, NULL, $5, $5)
             ON CONFLICT (tenant_id, idempotency_key) DO NOTHING
             RETURNING id, tenant_id, kind, state, idempotency_key, claimed_by, created_at, updated_at",
        )
        .bind(new.id.as_str())
        .bind(new.tenant.as_str())
        .bind(new.kind)
        .bind(new.idempotency_key)
        .bind(new.at.as_millis())
        .fetch_optional(&mut *tx)
        .await?;
        tx.commit().await?;
        if let Some(row) = row {
            return Ok(CreatedOperation {
                operation: map_operation(&row)?,
                replayed: false,
            });
        }
        // The key exists. Read it back in the same tenant: either the
        // identical operation (replay) or a distinct id (conflict).
        let existing = self
            .get_operation_by_key(new.tenant, new.idempotency_key)
            .await?
            .ok_or_else(|| StorageError::Conflict {
                what: "operation idempotency key",
                id: new.idempotency_key.to_owned(),
                reason: "row vanished while creating".to_owned(),
            })?;
        if existing.id != *new.id || existing.kind != new.kind {
            return Err(StorageError::Conflict {
                what: "operation idempotency key",
                id: new.idempotency_key.to_owned(),
                reason: format!(
                    "already bound to operation {} ({})",
                    existing.id, existing.kind
                ),
            });
        }
        Ok(CreatedOperation {
            operation: existing,
            replayed: true,
        })
    }

    /// Fetches one operation by id.
    pub async fn get_operation(
        &self,
        tenant: &TenantId,
        id: &OperationId,
    ) -> StorageResult<Operation> {
        let mut tx = self.begin_tenant(tenant).await?;
        let row = sqlx::query(
            "SELECT id, tenant_id, kind, state, idempotency_key, claimed_by, created_at, updated_at
             FROM operations WHERE tenant_id = $1 AND id = $2",
        )
        .bind(tenant.as_str())
        .bind(id.as_str())
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(|| StorageError::NotFound {
            what: "operation",
            id: id.as_str().to_owned(),
        })?;
        tx.commit().await?;
        map_operation(&row)
    }

    /// Fetches the operation bound to a tenant's idempotency key.
    pub async fn get_operation_by_key(
        &self,
        tenant: &TenantId,
        key: &str,
    ) -> StorageResult<Option<Operation>> {
        let mut tx = self.begin_tenant(tenant).await?;
        let row = sqlx::query(
            "SELECT id, tenant_id, kind, state, idempotency_key, claimed_by, created_at, updated_at
             FROM operations WHERE tenant_id = $1 AND idempotency_key = $2",
        )
        .bind(tenant.as_str())
        .bind(key)
        .fetch_optional(&mut *tx)
        .await?;
        tx.commit().await?;
        row.as_ref().map(map_operation).transpose()
    }

    /// Lists a tenant's operations, oldest first (id breaks ties).
    pub async fn list_operations(&self, tenant: &TenantId) -> StorageResult<Vec<Operation>> {
        let mut tx = self.begin_tenant(tenant).await?;
        let rows = sqlx::query(
            "SELECT id, tenant_id, kind, state, idempotency_key, claimed_by, created_at, updated_at
             FROM operations ORDER BY created_at, id",
        )
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;
        rows.iter().map(map_operation).collect()
    }

    /// Atomically claims a PENDING operation for `worker`, creating its
    /// next numbered attempt. Concurrent claims: only one succeeds; the
    /// loser gets a [`StorageError::Conflict`] naming the current state
    /// and holder. `at` is the claim instant, `deadline_at` the attempt
    /// deadline.
    pub async fn claim_operation(
        &self,
        tenant: &TenantId,
        id: &OperationId,
        worker: &WorkerId,
        attempt_id: &AttemptId,
        deadline_at: UnixMillis,
        at: UnixMillis,
    ) -> StorageResult<ClaimedOperation> {
        let mut tx = self.begin_tenant(tenant).await?;
        let row = sqlx::query(
            "UPDATE operations SET state = 'RUNNING', claimed_by = $3, updated_at = $4
             WHERE tenant_id = $1 AND id = $2 AND state = 'PENDING'
             RETURNING id, tenant_id, kind, state, idempotency_key, claimed_by, created_at, updated_at",
        )
        .bind(tenant.as_str())
        .bind(id.as_str())
        .bind(worker.as_str())
        .bind(at.as_millis())
        .fetch_optional(&mut *tx)
        .await?;
        let Some(row) = row else {
            // CAS lost: name the winner for fencing diagnostics.
            let current = self.operation_claimant(tenant, id).await?;
            return Err(StorageError::Conflict {
                what: "operation claim",
                id: id.as_str().to_owned(),
                reason: format!(
                    "state {} (claimed by {})",
                    current.0,
                    format_holder(current.1.as_deref())
                ),
            });
        };
        let operation = map_operation(&row)?;
        // Count attempts in the same transaction: attempt numbers are
        // contiguous and unique per operation even under concurrency.
        let next_no: i64 = sqlx::query_scalar(
            "SELECT COALESCE(MAX(attempt_no), 0) + 1 FROM operation_attempts
             WHERE tenant_id = $1 AND operation_id = $2",
        )
        .bind(tenant.as_str())
        .bind(id.as_str())
        .fetch_one(&mut *tx)
        .await?;
        let attempt_row = sqlx::query(
            "INSERT INTO operation_attempts
                 (id, tenant_id, operation_id, attempt_no, worker, outcome,
                  started_at, deadline_at)
             VALUES ($1, $2, $3, $4, $5, NULL, $6, $7)
             RETURNING id, tenant_id, operation_id, attempt_no, worker, outcome,
                       started_at, deadline_at",
        )
        .bind(attempt_id.as_str())
        .bind(tenant.as_str())
        .bind(id.as_str())
        .bind(next_no)
        .bind(worker.as_str())
        .bind(at.as_millis())
        .bind(deadline_at.as_millis())
        .fetch_one(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(ClaimedOperation {
            operation,
            attempt: map_attempt_record(&attempt_row)?,
        })
    }

    /// Closes an attempt. `Succeeded` and `Failed` move the operation
    /// to their matching terminal state; `TimedOut` returns it to
    /// PENDING so a future attempt can retry. Only the worker that
    /// currently holds the claim can close it (CAS on `claimed_by`).
    pub async fn close_attempt(
        &self,
        tenant: &TenantId,
        operation_id: &OperationId,
        attempt_id: &AttemptId,
        worker: &WorkerId,
        close: AttemptClose,
        at: UnixMillis,
    ) -> StorageResult<Operation> {
        let mut tx = self.begin_tenant(tenant).await?;
        let attempt = sqlx::query(
            "SELECT id, tenant_id, operation_id, attempt_no, worker, outcome,
                    started_at, deadline_at
             FROM operation_attempts WHERE tenant_id = $1 AND id = $2 AND worker = $3
             FOR UPDATE",
        )
        .bind(tenant.as_str())
        .bind(attempt_id.as_str())
        .bind(worker.as_str())
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(|| StorageError::NotFound {
            what: "operation attempt",
            id: attempt_id.as_str().to_owned(),
        })?;
        let belongs = attempt.try_get::<String, _>("operation_id")?;
        if belongs != operation_id.as_str() {
            return Err(StorageError::Invalid {
                what: "operation attempt",
                reason: format!(
                    "attempt {attempt_id} belongs to operation {belongs}, not {operation_id}"
                ),
            });
        }
        if attempt.try_get::<Option<String>, _>("outcome")?.is_some() {
            return Err(StorageError::Conflict {
                what: "operation attempt",
                id: attempt_id.as_str().to_owned(),
                reason: "already closed (duplicate completion)".to_owned(),
            });
        }
        sqlx::query(
            "UPDATE operation_attempts SET outcome = $3
             WHERE tenant_id = $1 AND id = $2 AND outcome IS NULL",
        )
        .bind(tenant.as_str())
        .bind(attempt_id.as_str())
        .bind(close.outcome().as_str())
        .execute(&mut *tx)
        .await?;
        let next_state = match close {
            AttemptClose::Succeeded => OperationState::Succeeded,
            AttemptClose::Failed => OperationState::Failed,
            AttemptClose::TimedOut => OperationState::Pending,
        };
        let next_str = next_state.as_str();
        let claim: Option<String> = if close == AttemptClose::TimedOut {
            None
        } else {
            Some(worker.as_str().to_owned())
        };
        let updated = sqlx::query(
            "UPDATE operations SET state = $4, claimed_by = $5, updated_at = $6
             WHERE tenant_id = $1 AND id = $2 AND state = 'RUNNING' AND claimed_by = $3",
        )
        .bind(tenant.as_str())
        .bind(operation_id.as_str())
        .bind(worker.as_str())
        .bind(next_str)
        .bind(claim)
        .bind(at.as_millis())
        .execute(&mut *tx)
        .await?
        .rows_affected();
        if updated != 1 {
            return Err(StorageError::Conflict {
                what: "operation completion",
                id: operation_id.as_str().to_owned(),
                reason: format!("claim lost (worker {worker} no longer holds it)"),
            });
        }
        tx.commit().await?;
        self.get_operation(tenant, operation_id).await
    }

    /// Reads `(state, claimed_by)` for claim diagnostics.
    async fn operation_claimant(
        &self,
        tenant: &TenantId,
        id: &OperationId,
    ) -> StorageResult<(String, Option<String>)> {
        let mut tx = self.begin_tenant(tenant).await?;
        let row = sqlx::query(
            "SELECT state, claimed_by FROM operations WHERE tenant_id = $1 AND id = $2",
        )
        .bind(tenant.as_str())
        .bind(id.as_str())
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(|| StorageError::NotFound {
            what: "operation",
            id: id.as_str().to_owned(),
        })?;
        tx.commit().await?;
        Ok((row.try_get("state")?, row.try_get("claimed_by")?))
    }

    /// Attempts of one operation, by number.
    pub async fn list_attempts(
        &self,
        tenant: &TenantId,
        operation_id: &OperationId,
    ) -> StorageResult<Vec<AttemptRecord>> {
        let mut tx = self.begin_tenant(tenant).await?;
        let rows = sqlx::query(
            "SELECT id, tenant_id, operation_id, attempt_no, worker, outcome,
                    started_at, deadline_at
             FROM operation_attempts
             WHERE tenant_id = $1 AND operation_id = $2 ORDER BY attempt_no",
        )
        .bind(tenant.as_str())
        .bind(operation_id.as_str())
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;
        rows.iter().map(map_attempt_record).collect()
    }

    /// In-flight attempts (no outcome yet) that have crossed their
    /// deadline. Idempotent read: the reaper can re-run safely.
    pub async fn expired_attempts(
        &self,
        tenant: &TenantId,
        now: UnixMillis,
    ) -> StorageResult<Vec<ExpiredAttempt>> {
        let mut tx = self.begin_tenant(tenant).await?;
        let rows = sqlx::query(
            "SELECT a.id, a.tenant_id, a.operation_id, a.attempt_no, a.worker, a.outcome,
                    a.started_at, a.deadline_at
             FROM operation_attempts a
             JOIN operations o ON o.tenant_id = a.tenant_id AND o.id = a.operation_id
             WHERE a.tenant_id = $1 AND a.outcome IS NULL AND a.deadline_at <= $2
                   AND o.state = 'RUNNING'
             ORDER BY a.deadline_at, a.id",
        )
        .bind(tenant.as_str())
        .bind(now.as_millis())
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;
        rows.iter().map(map_expired).collect()
    }
}

/// Formats a soft-referenced holder for conflict diagnostics.
fn format_holder(holder: Option<&str>) -> String {
    holder.map_or_else(|| "nobody".to_owned(), str::to_owned)
}

fn map_operation(row: &sqlx::postgres::PgRow) -> StorageResult<Operation> {
    let state = row.try_get::<String, _>("state")?;
    Ok(Operation {
        id: OperationId::parse(row.try_get::<String, _>("id")?)?,
        tenant_id: TenantId::parse(row.try_get::<String, _>("tenant_id")?)?,
        kind: row.try_get("kind")?,
        state: parse_state(&state)?,
        created_at: UnixMillis::from_millis(row.try_get::<i64, _>("created_at")?)?,
    })
}

fn parse_state(raw: &str) -> StorageResult<OperationState> {
    match raw {
        "PENDING" => Ok(OperationState::Pending),
        "RUNNING" => Ok(OperationState::Running),
        "SUCCEEDED" => Ok(OperationState::Succeeded),
        "FAILED" => Ok(OperationState::Failed),
        "CANCELLED" => Ok(OperationState::Cancelled),
        other => Err(StorageError::Invalid {
            what: "operation state",
            reason: format!("unknown state {other}"),
        }),
    }
}

fn parse_outcome(raw: &Option<String>) -> StorageResult<Option<AttemptOutcome>> {
    match raw.as_deref() {
        None => Ok(None),
        Some("succeeded") => Ok(Some(AttemptOutcome::Succeeded)),
        Some("failed") => Ok(Some(AttemptOutcome::Failed)),
        Some("timed out") => Ok(Some(AttemptOutcome::TimedOut)),
        Some(other) => Err(StorageError::Invalid {
            what: "attempt outcome",
            reason: format!("unknown outcome {other}"),
        }),
    }
}

fn map_attempt_record(row: &sqlx::postgres::PgRow) -> StorageResult<AttemptRecord> {
    let no: i64 = row.try_get("attempt_no")?;
    Ok(AttemptRecord {
        attempt_id: AttemptId::parse(row.try_get::<String, _>("id")?)?,
        operation_id: OperationId::parse(row.try_get::<String, _>("operation_id")?)?,
        attempt_no: u32::try_from(no).map_err(|_| StorageError::Invalid {
            what: "attempt number",
            reason: format!("{no} exceeds u32"),
        })?,
        worker: row
            .try_get::<Option<String>, _>("worker")?
            .map(WorkerId::parse)
            .transpose()?,
        outcome: parse_outcome(&row.try_get("outcome")?)?,
        started_at: UnixMillis::from_millis(row.try_get::<i64, _>("started_at")?)?,
        deadline_at: UnixMillis::from_millis(row.try_get::<i64, _>("deadline_at")?)?,
    })
}

fn map_expired(row: &sqlx::postgres::PgRow) -> StorageResult<ExpiredAttempt> {
    Ok(ExpiredAttempt {
        attempt: map_attempt_record(row)?,
    })
}
