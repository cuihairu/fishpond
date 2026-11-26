//! F014 acceptance: operation persistence, single claim, deadline reaping
//! and idempotency — the four storage invariants the scheduler and worker
//! build on.
//!
//! Idempotency lives in the unique `(tenant_id, idempotency_key)` key: a
//! create that hits the key returns the original operation, and a
//! *different* id under the same key is a conflict rather than an
//! silent alias. Claim is a CAS (`state = 'PENDING'` in the WHERE), so
//! exactly one of two concurrent claimants succeeds and the loser gets a
//! conflict naming the winner. Attempts are numbered from one; closing
//! one twice is rejected; a timed-out attempt reopens the operation
//! while a succeeded or failed one is terminal. Deadline reaping reads
//! only in-flight attempts whose deadline passed while the operation is
//! still running, so closing an attempt before its deadline takes it out
//! of the reaper's view. Every surface is tenant-scoped under the same
//! row-level security as the rest of the schema.

mod common;

use common::{ENV, gated, tenant_record, ts, unique_entity, unique_tenant};
use fishpond_domain::Operation;
use fishpond_domain::{
    AttemptId, AttemptOutcome, OperationId, OperationState, TenantId, UnixMillis, WorkerId,
};
use fishpond_storage::{AttemptClose, NewOperation, Store};
use sqlx::Row;

fn operation_id(tag: &str, n: u32) -> OperationId {
    OperationId::parse(unique_entity("op", &format!("{tag}-{n}"))).expect("operation id")
}

fn attempt_id(tag: &str, n: u32) -> AttemptId {
    AttemptId::parse(unique_entity("att", &format!("{tag}-{n}"))).expect("attempt id")
}

fn worker_id(tag: &str, n: u32) -> WorkerId {
    WorkerId::parse(unique_entity("wrk", &format!("{tag}-{n}"))).expect("worker id")
}

/// Creates a PENDING operation under `tenant` with a unique key.
async fn create_pending(
    store: &Store,
    tenant: &TenantId,
    tag: &str,
    n: u32,
    at: UnixMillis,
) -> Operation {
    let id = operation_id(tag, n);
    store
        .create_operation(&NewOperation {
            id: &id,
            tenant,
            kind: "send_message",
            idempotency_key: &unique_entity("key", &format!("{tag}-{n}")),
            at,
        })
        .await
        .expect("create operation")
        .operation
}

#[tokio::test]
async fn idempotency_key_replays_and_conflicts() {
    let Some(store) = gated().await else {
        eprintln!("skip: {ENV} is not set (CI has no database)");
        return;
    };
    let tenant = unique_tenant("op-idem");
    store
        .upsert_tenant(&tenant_record(&tenant, "op-idem"))
        .await
        .expect("tenant insert");

    // First create with the key: fresh operation, `replayed` false.
    let id = operation_id("idem", 1);
    let key = unique_entity("key", "idem-1");
    let created = store
        .create_operation(&NewOperation {
            id: &id,
            tenant: &tenant,
            kind: "send_message",
            idempotency_key: &key,
            at: ts(1_000),
        })
        .await
        .expect("first create");
    assert!(!created.replayed);
    assert_eq!(created.operation.id, id);
    assert_eq!(created.operation.state, OperationState::Pending);

    // Re-create with the *same* id and kind: replays the original,
    // never duplicates it.
    let replayed = store
        .create_operation(&NewOperation {
            id: &id,
            tenant: &tenant,
            kind: "send_message",
            idempotency_key: &key,
            at: ts(2_000),
        })
        .await
        .expect("replay create");
    assert!(replayed.replayed);
    assert_eq!(replayed.operation.id, id);
    assert_eq!(replayed.operation.created_at, created.operation.created_at);

    // A *different* id under the same key is a conflict: keys must not
    // silently alias distinct intents.
    let other = operation_id("idem", 2);
    let err = store
        .create_operation(&NewOperation {
            id: &other,
            tenant: &tenant,
            kind: "send_message",
            idempotency_key: &key,
            at: ts(3_000),
        })
        .await
        .expect_err("distinct id must conflict");
    assert!(
        err.to_string().contains("already bound to operation"),
        "conflict names the bound operation: {err}"
    );

    // An empty key is rejected before touching the database.
    let err = store
        .create_operation(&NewOperation {
            id: &operation_id("idem", 3),
            tenant: &tenant,
            kind: "send_message",
            idempotency_key: "",
            at: ts(4_000),
        })
        .await
        .expect_err("empty key must be rejected");
    assert!(err.to_string().contains("empty"), "reject empty key: {err}");
}

#[tokio::test]
async fn claim_cas_loses_to_the_current_holder() {
    let Some(store) = gated().await else {
        eprintln!("skip: {ENV} is not set (CI has no database)");
        return;
    };
    let tenant = unique_tenant("op-claim");
    store
        .upsert_tenant(&tenant_record(&tenant, "op-claim"))
        .await
        .expect("tenant insert");
    let op = create_pending(&store, &tenant, "claim", 1, ts(1_000)).await;

    // First claimant wins the CAS and opens attempt #1.
    let winner = worker_id("claim", 1);
    let claimed = store
        .claim_operation(
            &tenant,
            &op.id,
            &winner,
            &attempt_id("claim", 1),
            ts(5_000),
            ts(2_000),
        )
        .await
        .expect("first claim");
    assert_eq!(claimed.operation.state, OperationState::Running);
    assert_eq!(claimed.attempt.attempt_no, 1);
    assert_eq!(claimed.attempt.outcome, None);
    assert_eq!(claimed.attempt.deadline_at, ts(5_000));

    // A second claimant on the now-RUNNING operation loses the CAS; the
    // conflict names the winner for fencing diagnostics.
    let loser = worker_id("claim", 2);
    let err = store
        .claim_operation(
            &tenant,
            &op.id,
            &loser,
            &attempt_id("claim", 2),
            ts(5_000),
            ts(2_000),
        )
        .await
        .expect_err("second claim must lose the CAS");
    let message = err.to_string();
    assert!(
        message.contains("RUNNING"),
        "names the live state: {message}"
    );
    assert!(
        message.contains(winner.as_str()),
        "names the winner: {message}"
    );

    // Claiming an unknown operation is a not-found, not a conflict.
    let missing = operation_id("claim", 99);
    let err = store
        .claim_operation(
            &tenant,
            &missing,
            &loser,
            &attempt_id("claim", 3),
            ts(5_000),
            ts(2_000),
        )
        .await
        .expect_err("missing operation");
    assert!(err.to_string().contains("not found"));
}

#[tokio::test]
async fn attempts_number_from_one_and_close_once() {
    let Some(store) = gated().await else {
        eprintln!("skip: {ENV} is not set (CI has no database)");
        return;
    };
    let tenant = unique_tenant("op-attempts");
    store
        .upsert_tenant(&tenant_record(&tenant, "op-attempts"))
        .await
        .expect("tenant insert");
    let op = create_pending(&store, &tenant, "att", 1, ts(1_000)).await;
    let worker = worker_id("att", 1);

    // First attempt: claim, then close as timed out — the operation
    // returns to PENDING so a later attempt can retry.
    let first = store
        .claim_operation(
            &tenant,
            &op.id,
            &worker,
            &attempt_id("att", 1),
            ts(5_000),
            ts(2_000),
        )
        .await
        .expect("first claim");
    let reopened = store
        .close_attempt(
            &tenant,
            &op.id,
            &first.attempt.attempt_id,
            &worker,
            AttemptClose::TimedOut,
            ts(4_000),
        )
        .await
        .expect("close timed out");
    assert_eq!(reopened.state, OperationState::Pending);

    // Second attempt carries the next number.
    let second = store
        .claim_operation(
            &tenant,
            &op.id,
            &worker,
            &attempt_id("att", 2),
            ts(9_000),
            ts(5_000),
        )
        .await
        .expect("second claim");
    assert_eq!(second.attempt.attempt_no, 2);

    // Closing the second attempt twice is a duplicate-completion
    // conflict: the first close succeeds, the second is rejected.
    let done = store
        .close_attempt(
            &tenant,
            &op.id,
            &second.attempt.attempt_id,
            &worker,
            AttemptClose::Succeeded,
            ts(6_000),
        )
        .await
        .expect("first close succeeds");
    assert_eq!(done.state, OperationState::Succeeded);
    let err = store
        .close_attempt(
            &tenant,
            &op.id,
            &second.attempt.attempt_id,
            &worker,
            AttemptClose::Succeeded,
            ts(7_000),
        )
        .await
        .expect_err("second close must be rejected");
    assert!(
        err.to_string().contains("already closed"),
        "duplicate completion is rejected: {err}"
    );
    // An attempt that belongs to another operation cannot close this one.
    let stray = store
        .claim_operation(
            &tenant,
            &create_pending(&store, &tenant, "att", 2, ts(1_000))
                .await
                .id,
            &worker,
            &attempt_id("att", 3),
            ts(9_000),
            ts(6_000),
        )
        .await
        .expect("claim on the second operation");
    let err = store
        .close_attempt(
            &tenant,
            &op.id,
            &stray.attempt.attempt_id,
            &worker,
            AttemptClose::Succeeded,
            ts(7_000),
        )
        .await
        .expect_err("foreign attempt must not close this operation");
    assert!(err.to_string().contains("belongs to operation"));

    // The attempts read back in order with their outcomes recorded.
    let attempts = store
        .list_attempts(&tenant, &op.id)
        .await
        .expect("list attempts");
    assert_eq!(attempts.len(), 2);
    assert_eq!(attempts[0].attempt_no, 1);
    assert_eq!(attempts[0].outcome, Some(AttemptOutcome::TimedOut));
    assert_eq!(attempts[1].attempt_no, 2);
    assert_eq!(attempts[1].outcome, Some(AttemptOutcome::Succeeded));
}

#[tokio::test]
async fn terminal_close_and_non_holder_are_rejected() {
    let Some(store) = gated().await else {
        eprintln!("skip: {ENV} is not set (CI has no database)");
        return;
    };
    let tenant = unique_tenant("op-close");
    store
        .upsert_tenant(&tenant_record(&tenant, "op-close"))
        .await
        .expect("tenant insert");
    let op = create_pending(&store, &tenant, "close", 1, ts(1_000)).await;
    let holder = worker_id("close", 1);
    let other = worker_id("close", 2);

    let claimed = store
        .claim_operation(
            &tenant,
            &op.id,
            &holder,
            &attempt_id("close", 1),
            ts(5_000),
            ts(2_000),
        )
        .await
        .expect("claim");

    // A worker that does not hold the claim cannot close it.
    let err = store
        .close_attempt(
            &tenant,
            &op.id,
            &claimed.attempt.attempt_id,
            &other,
            AttemptClose::Succeeded,
            ts(3_000),
        )
        .await
        .expect_err("non-holder must not close");
    assert!(err.to_string().contains("not found"));

    // Succeeding moves the operation to the terminal SUCCEEDED state and
    // keeps the outcome on the attempt.
    let done = store
        .close_attempt(
            &tenant,
            &op.id,
            &claimed.attempt.attempt_id,
            &holder,
            AttemptClose::Succeeded,
            ts(3_000),
        )
        .await
        .expect("close succeeded");
    assert_eq!(done.state, OperationState::Succeeded);
    let attempts = store.list_attempts(&tenant, &op.id).await.expect("list");
    assert_eq!(attempts[0].outcome, Some(AttemptOutcome::Succeeded));

    // A terminal operation can never be claimed again.
    let err = store
        .claim_operation(
            &tenant,
            &op.id,
            &other,
            &attempt_id("close", 2),
            ts(9_000),
            ts(4_000),
        )
        .await
        .expect_err("terminal operation cannot be claimed");
    assert!(err.to_string().contains("SUCCEEDED"));
}

#[tokio::test]
async fn deadline_reaping_reads_only_unclosed_in_flight() {
    let Some(store) = gated().await else {
        eprintln!("skip: {ENV} is not set (CI has no database)");
        return;
    };
    let tenant = unique_tenant("op-deadline");
    store
        .upsert_tenant(&tenant_record(&tenant, "op-deadline"))
        .await
        .expect("tenant insert");
    let worker = worker_id("deadline", 1);

    // Operation A: claimed, in flight, deadline already passed at t=4_000.
    let a = create_pending(&store, &tenant, "deadline", 1, ts(1_000)).await;
    let a_claim = store
        .claim_operation(
            &tenant,
            &a.id,
            &worker,
            &attempt_id("deadline", 1),
            ts(3_000),
            ts(2_000),
        )
        .await
        .expect("claim A");

    // Operation B: claimed, in flight, deadline still in the future.
    let b = create_pending(&store, &tenant, "deadline", 2, ts(1_000)).await;
    let _b_claim = store
        .claim_operation(
            &tenant,
            &b.id,
            &worker,
            &attempt_id("deadline", 2),
            ts(9_000),
            ts(2_000),
        )
        .await
        .expect("claim B");

    // Operation C: deadline passed, but the attempt was closed before the
    // reaper ran — it is no longer in flight and must not be reaped.
    let c = create_pending(&store, &tenant, "deadline", 3, ts(1_000)).await;
    let c_claim = store
        .claim_operation(
            &tenant,
            &c.id,
            &worker,
            &attempt_id("deadline", 3),
            ts(3_000),
            ts(2_000),
        )
        .await
        .expect("claim C");
    store
        .close_attempt(
            &tenant,
            &c.id,
            &c_claim.attempt.attempt_id,
            &worker,
            AttemptClose::Failed,
            ts(2_500),
        )
        .await
        .expect("close C");

    // Reap at t=4_000: only A is due.
    let expired = store
        .expired_attempts(&tenant, ts(4_000))
        .await
        .expect("reap");
    assert_eq!(
        expired.len(),
        1,
        "only the in-flight, past-deadline attempt"
    );
    assert_eq!(expired[0].attempt.operation_id, a.id);
    assert_eq!(expired[0].attempt.deadline_at, ts(3_000));
    assert_eq!(expired[0].attempt.outcome, None);

    // The reaper is an idempotent read: re-running it returns the same
    // row until the attempt is actually closed.
    let again = store
        .expired_attempts(&tenant, ts(4_000))
        .await
        .expect("reap again");
    assert_eq!(again.len(), 1);

    // Closing A as timed out takes it out of the reaper's view and
    // reopens the operation for a fresh attempt.
    let reopened = store
        .close_attempt(
            &tenant,
            &a.id,
            &a_claim.attempt.attempt_id,
            &worker,
            AttemptClose::TimedOut,
            ts(4_000),
        )
        .await
        .expect("reap A");
    assert_eq!(reopened.state, OperationState::Pending);
    assert_eq!(
        store
            .expired_attempts(&tenant, ts(5_000))
            .await
            .expect("reap after")
            .len(),
        0
    );

    // B's deadline passes later and it is still in flight: now it is due.
    let expired = store
        .expired_attempts(&tenant, ts(10_000))
        .await
        .expect("reap B");
    assert_eq!(expired.len(), 1);
    assert_eq!(expired[0].attempt.operation_id, b.id);
}

#[tokio::test]
async fn operations_are_tenant_isolated() {
    let Some(store) = gated().await else {
        eprintln!("skip: {ENV} is not set (CI has no database)");
        return;
    };
    let tenant = unique_tenant("op-iso-a");
    let other = unique_tenant("op-iso-b");
    store
        .upsert_tenant(&tenant_record(&tenant, "op-iso-a"))
        .await
        .expect("tenant A insert");
    store
        .upsert_tenant(&tenant_record(&other, "op-iso-b"))
        .await
        .expect("tenant B insert");
    let op = create_pending(&store, &tenant, "iso", 1, ts(1_000)).await;

    // The bare pool sees nothing of the tenant's operations, and a
    // foreign tenant cannot claim or reap them.
    let pool = store.pool();
    let bare: i64 = sqlx::query("SELECT count(*) FROM operations")
        .fetch_one(pool)
        .await
        .expect("bare count")
        .try_get("count")
        .expect("count column");
    assert_eq!(bare, 0, "no tenant rows leak to the bare pool");

    let err = store
        .claim_operation(
            &other,
            &op.id,
            &worker_id("iso", 1),
            &attempt_id("iso", 1),
            ts(9_000),
            ts(2_000),
        )
        .await
        .expect_err("cross-tenant claim");
    assert!(err.to_string().contains("not found"));

    let err = store
        .get_operation(&other, &op.id)
        .await
        .expect_err("cross-tenant read");
    assert!(err.to_string().contains("not found"));

    // The owning tenant still sees its own operation.
    let mine = store
        .get_operation(&tenant, &op.id)
        .await
        .expect("own read");
    assert_eq!(mine.id, op.id);
}
