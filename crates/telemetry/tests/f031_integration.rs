//! F031 integration face: the whole pipeline over real rows —
//! operations claimed and closed through the state machine, events
//! published and consumed (with gap-injection and replay chaos), and
//! the telemetry depth snapshot as the independent observer. Env-gated
//! like every DB-backed suite.

mod common;

use common::{ENV, gated, now_ms, tenant_record, ts, unique_entity, unique_tenant};
use fishpond_domain::{AttemptId, EventId, EventPayload, OperationId, TenantId, WorkerId};
use fishpond_events::EventService;
use fishpond_storage::{AttemptClose, NewEvent, NewOperation, Store};
use fishpond_telemetry::{AlertThresholds, collect_queue_depth, evaluate};

/// Creates a PENDING operation and returns its id.
async fn create_op(store: &Store, tenant: &TenantId, tag: &str, n: u32) -> OperationId {
    let id = OperationId::parse(unique_entity("op", &format!("{tag}-{n}"))).expect("operation id");
    store
        .create_operation(&NewOperation {
            id: &id,
            tenant,
            kind: "send_message",
            idempotency_key: &unique_entity("key", &format!("{tag}-{n}")),
            at: ts(1_000),
        })
        .await
        .expect("create operation");
    id
}

/// Claims `op` for `worker`, returns the attempt id.
async fn claim(
    store: &Store,
    tenant: &TenantId,
    op: &OperationId,
    worker: &WorkerId,
    attempt: &AttemptId,
) {
    store
        .claim_operation(
            tenant,
            op,
            worker,
            attempt,
            ts(now_ms() + 60_000),
            ts(now_ms()),
        )
        .await
        .expect("claim operation");
}

/// Appends `n` test events; returns how many were written.
async fn append_ticks(store: &Store, tenant: &TenantId, tag: &str, n: u32) {
    let payload = EventPayload::parse(r#"{"kind":"tick"}"#).expect("payload");
    let ids: Vec<EventId> = (1..=n)
        .map(|k| EventId::parse(unique_entity("evt", &format!("{tag}-{k}"))).expect("event id"))
        .collect();
    let events: Vec<NewEvent<'_>> = ids
        .iter()
        .map(|id| NewEvent {
            id,
            event_type: "test.tick",
            account_id: None,
            occurred_at: ts(2_000),
            payload: &payload,
        })
        .collect();
    let envelopes = store
        .append_events(tenant, &events)
        .await
        .expect("append events");
    assert_eq!(envelopes.len(), usize::try_from(n).expect("n fits"));
}

#[tokio::test]
async fn pipeline_flows_end_to_end_and_the_observer_sees_calm() {
    let Some(store) = gated().await else {
        eprintln!("skip: {ENV} is not set (CI has no database)");
        return;
    };
    let tenant = unique_tenant("f031-e2e");
    store
        .upsert_tenant(&tenant_record(&tenant, "f031-e2e"))
        .await
        .expect("tenant insert");

    // One operation succeeds, one fails terminally, events flow.
    let won = create_op(&store, &tenant, "e2e-w", 1).await;
    let lost = create_op(&store, &tenant, "e2e-l", 2).await;
    let worker = WorkerId::parse(unique_entity("wrk", "e2e")).expect("worker id");
    let won_attempt = AttemptId::parse(unique_entity("att", "e2e-w")).expect("attempt id");
    let lost_attempt = AttemptId::parse(unique_entity("att", "e2e-l")).expect("attempt id");
    claim(&store, &tenant, &won, &worker, &won_attempt).await;
    claim(&store, &tenant, &lost, &worker, &lost_attempt).await;
    store
        .close_attempt(
            &tenant,
            &won,
            &won_attempt,
            &worker,
            AttemptClose::Succeeded,
            ts(4_000),
        )
        .await
        .expect("close succeeded");
    store
        .close_attempt(
            &tenant,
            &lost,
            &lost_attempt,
            &worker,
            AttemptClose::Failed,
            ts(4_000),
        )
        .await
        .expect("close failed");

    append_ticks(&store, &tenant, "e2e", 3).await;
    let events = EventService::new(store.clone());
    let envelopes = {
        // A page of the log for the consumer: everything the tenant has.
        store
            .list_events(&tenant, 0, 100)
            .await
            .expect("list events")
    };
    let report = events
        .consume_batch(&tenant, "projection", &envelopes, ts(5_000), |_env| Ok(()))
        .await
        .expect("consume");
    assert_eq!(report.applied.len(), 3);
    assert_eq!(report.checkpoint, 3);

    // The observer: everything terminal, consumer caught up — calm.
    let depth = collect_queue_depth(&store, &tenant, "projection")
        .await
        .expect("collect");
    assert_eq!(depth.backlog(), 0, "SUCCEEDED and FAILED are not backlog");
    assert_eq!(depth.in_flight_attempts, 0);
    assert_eq!(depth.consumer_lag, 0);
    assert!(evaluate(&depth, &[], &AlertThresholds::default()).is_empty());
}

#[tokio::test]
async fn timed_out_attempts_reopen_operations_for_retry() {
    let Some(store) = gated().await else {
        eprintln!("skip: {ENV} is not set (CI has no database)");
        return;
    };
    let tenant = unique_tenant("f031-retry");
    store
        .upsert_tenant(&tenant_record(&tenant, "f031-retry"))
        .await
        .expect("tenant insert");
    let op = create_op(&store, &tenant, "retry", 1).await;
    let worker = WorkerId::parse(unique_entity("wrk", "retry-1")).expect("worker id");
    let first = AttemptId::parse(unique_entity("att", "retry-1")).expect("attempt id");
    claim(&store, &tenant, &op, &worker, &first).await;

    // Chaos: the attempt times out — the operation must come back for
    // another round, not die.
    store
        .close_attempt(
            &tenant,
            &op,
            &first,
            &worker,
            AttemptClose::TimedOut,
            ts(9_000),
        )
        .await
        .expect("close timed out");
    let depth = collect_queue_depth(&store, &tenant, "projection")
        .await
        .expect("collect");
    assert_eq!(depth.backlog(), 1, "a timed-out operation is work again");
    assert_eq!(
        depth.in_flight_attempts, 0,
        "the closed attempt has an outcome"
    );

    // Retry: a second worker picks it up as attempt 2 and succeeds.
    let worker2 = WorkerId::parse(unique_entity("wrk", "retry-2")).expect("worker id");
    let second = AttemptId::parse(unique_entity("att", "retry-2")).expect("attempt id");
    let claimed = store
        .claim_operation(
            &tenant,
            &op,
            &worker2,
            &second,
            ts(now_ms() + 60_000),
            ts(now_ms()),
        )
        .await
        .expect("re-claim after timeout");
    assert_eq!(claimed.attempt.attempt_no, 2);
    store
        .close_attempt(
            &tenant,
            &op,
            &second,
            &worker2,
            AttemptClose::Succeeded,
            ts(12_000),
        )
        .await
        .expect("close retry");

    let attempts = store.list_attempts(&tenant, &op).await.expect("attempts");
    assert_eq!(attempts.len(), 2);
    assert_eq!(
        attempts[0].outcome,
        Some(fishpond_domain::AttemptOutcome::TimedOut)
    );
    assert_eq!(
        attempts[1].outcome,
        Some(fishpond_domain::AttemptOutcome::Succeeded)
    );
}

#[tokio::test]
async fn gap_injection_holds_the_checkpoint_and_replays_cleanly() {
    let Some(store) = gated().await else {
        eprintln!("skip: {ENV} is not set (CI has no database)");
        return;
    };
    let tenant = unique_tenant("f031-gap");
    store
        .upsert_tenant(&tenant_record(&tenant, "f031-gap"))
        .await
        .expect("tenant insert");
    append_ticks(&store, &tenant, "gap", 3).await;

    let events = EventService::new(store.clone());
    let envelopes = store
        .list_events(&tenant, 0, 100)
        .await
        .expect("list events");
    assert_eq!(envelopes.len(), 3);

    // Chaos: the transport drops seq 2 — the consumer must apply
    // nothing and hold the checkpoint.
    let torn: Vec<_> = envelopes
        .iter()
        .filter(|env| env.seq != 2)
        .cloned()
        .collect();
    let report = events
        .consume_batch(&tenant, "projection", &torn, ts(5_000), |_env| Ok(()))
        .await
        .expect("consume reports gaps, never applies them");
    assert!(report.applied.is_empty());
    assert_eq!(report.missing_seqs, vec![2]);
    assert_eq!(report.checkpoint, 0);

    // The full batch replays cleanly and certifies the prefix.
    let report = events
        .consume_batch(&tenant, "projection", &envelopes, ts(6_000), |_env| Ok(()))
        .await
        .expect("consume full batch");
    assert_eq!(report.applied.len(), 3);
    assert_eq!(report.checkpoint, 3);

    // Redelivery of the same batch: everything at or below the
    // checkpoint is certified-skipped — nothing re-applies and the
    // checkpoint does not move (idempotent under at-least-once
    // transport; duplicates are only for re-deliveries still above the
    // checkpoint).
    let report = events
        .consume_batch(&tenant, "projection", &envelopes, ts(7_000), |_env| Ok(()))
        .await
        .expect("redelivery");
    assert!(report.applied.is_empty());
    assert!(report.duplicates.is_empty());
    assert_eq!(report.checkpoint, 3);
}

#[tokio::test]
async fn racing_claims_elect_exactly_one_worker() {
    let Some(store) = gated().await else {
        eprintln!("skip: {ENV} is not set (CI has no database)");
        return;
    };
    let tenant = unique_tenant("f031-race");
    store
        .upsert_tenant(&tenant_record(&tenant, "f031-race"))
        .await
        .expect("tenant insert");
    let op = create_op(&store, &tenant, "race", 1).await;

    let store_a = store.clone();
    let store_b = store.clone();
    let tenant_a = tenant.clone();
    let tenant_b = tenant.clone();
    let worker_a = WorkerId::parse(unique_entity("wrk", "race-a")).expect("worker id");
    let worker_b = WorkerId::parse(unique_entity("wrk", "race-b")).expect("worker id");
    let attempt_a = AttemptId::parse(unique_entity("att", "race-a")).expect("attempt id");
    let attempt_b = AttemptId::parse(unique_entity("att", "race-b")).expect("attempt id");
    let op_a = op.clone();
    let op_b = op.clone();

    let (left, right) = tokio::join!(
        async move {
            store_a
                .claim_operation(
                    &tenant_a,
                    &op_a,
                    &worker_a,
                    &attempt_a,
                    ts(now_ms() + 60_000),
                    ts(now_ms()),
                )
                .await
        },
        async move {
            store_b
                .claim_operation(
                    &tenant_b,
                    &op_b,
                    &worker_b,
                    &attempt_b,
                    ts(now_ms() + 60_000),
                    ts(now_ms()),
                )
                .await
        },
    );
    // Exactly one CAS win; the loser gets a named conflict.
    let outcomes = [left.is_ok(), right.is_ok()];
    assert_eq!(outcomes.iter().filter(|ok| **ok).count(), 1, "{outcomes:?}");
    let loser_err = if left.is_ok() {
        right.unwrap_err()
    } else {
        left.unwrap_err()
    };
    assert!(matches!(
        loser_err,
        fishpond_storage::StorageError::Conflict { .. }
    ));

    // One attempt exists; the operation is RUNNING exactly once.
    let depth = collect_queue_depth(&store, &tenant, "projection")
        .await
        .expect("collect");
    assert_eq!(depth.in_flight_attempts, 1);
    assert_eq!(depth.backlog(), 1);
}
