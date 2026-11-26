//! F030 acceptance (queue-depth + alert face): the collector reads real
//! rows (operations by state, in-flight/overdue attempts, consumer lag),
//! tenant isolation holds, and firing rules open idempotent monitor
//! incidents. Env-gated like every DB-backed suite.

mod common;

use common::{ENV, gated, now_ms, tenant_record, ts, unique_entity, unique_tenant};
use fishpond_domain::{AttemptId, EventId, EventPayload, OperationId, TenantId, WorkerId};
use fishpond_monitor::IncidentLog;
use fishpond_storage::{NewEvent, NewOperation, Store};
use fishpond_telemetry::{AlertThresholds, MailboxDepthSample, collect_queue_depth, evaluate};
use std::sync::Arc;

fn operation_id(tag: &str, n: u32) -> OperationId {
    OperationId::parse(unique_entity("op", &format!("{tag}-{n}"))).expect("operation id")
}

fn attempt_id(tag: &str, n: u32) -> AttemptId {
    AttemptId::parse(unique_entity("att", &format!("{tag}-{n}"))).expect("attempt id")
}

fn worker_id(tag: &str, n: u32) -> WorkerId {
    WorkerId::parse(unique_entity("wrk", &format!("{tag}-{n}"))).expect("worker id")
}

/// Registers `tenant`, creates `pending` PENDING operations and claims
/// `in_flight` of them (the last one already past its deadline).
async fn seed_operations(
    store: &Store,
    tenant: &TenantId,
    tag: &str,
    pending: u32,
    in_flight: u32,
) {
    store
        .upsert_tenant(&tenant_record(tenant, tag))
        .await
        .expect("tenant insert");
    let worker = worker_id(tag, 0);
    for n in 1..=pending {
        store
            .create_operation(&NewOperation {
                id: &operation_id(tag, n),
                tenant,
                kind: "send_message",
                idempotency_key: &unique_entity("key", &format!("{tag}-p-{n}")),
                at: ts(1_000),
            })
            .await
            .expect("create operation");
    }
    for n in 1..=in_flight {
        let op = operation_id(tag, pending + n);
        store
            .create_operation(&NewOperation {
                id: &op,
                tenant,
                kind: "send_message",
                idempotency_key: &unique_entity("key", &format!("{tag}-i-{n}")),
                at: ts(1_000),
            })
            .await
            .expect("create claimed operation");
        // The overdue probe uses the real clock, so deadlines are
        // relative to now: the last attempt is already past its
        // deadline, the others have an hour.
        let now = now_ms();
        let deadline = if n == in_flight {
            now - 1_000
        } else {
            now + 60_000
        };
        store
            .claim_operation(
                tenant,
                &op,
                &worker,
                &attempt_id(tag, n),
                ts(deadline),
                ts(now),
            )
            .await
            .expect("claim operation");
    }
}

#[tokio::test]
async fn collector_reads_operations_attempts_and_lag() {
    let Some(store) = gated().await else {
        eprintln!("skip: {ENV} is not set (CI has no database)");
        return;
    };
    let tenant = unique_tenant("td-depth");
    seed_operations(&store, &tenant, "td-depth", 3, 2).await;

    // Two events; the consumer certifies only the first, so lag is 1.
    let payload = EventPayload::parse(r#"{"kind":"test"}"#).expect("payload");
    let ids: Vec<EventId> = (1..=2)
        .map(|n| EventId::parse(unique_entity("evt", &format!("{n}"))).expect("event id"))
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
    let envelopes = store.append_events(&tenant, &events).await.expect("append");
    assert_eq!(envelopes.len(), 2);
    store
        .advance_checkpoint(&tenant, "projection", 1, ts(3_000))
        .await
        .expect("checkpoint");

    let report = collect_queue_depth(&store, &tenant, "projection")
        .await
        .expect("collect");
    assert_eq!(report.tenant, tenant.as_str());
    assert_eq!(report.backlog(), 5, "three pending + two running");
    assert_eq!(report.in_flight_attempts, 2);
    assert_eq!(report.overdue_attempts, 1);
    assert_eq!(report.consumer_lag, 1);

    // A caught-up consumer has no lag at all.
    store
        .advance_checkpoint(&tenant, "caught-up", 2, ts(3_500))
        .await
        .expect("checkpoint");
    let report = collect_queue_depth(&store, &tenant, "caught-up")
        .await
        .expect("collect");
    assert_eq!(report.consumer_lag, 0);
}

#[tokio::test]
async fn depth_is_tenant_isolated() {
    let Some(store) = gated().await else {
        eprintln!("skip: {ENV} is not set (CI has no database)");
        return;
    };
    let mine = unique_tenant("td-iso-mine");
    let theirs = unique_tenant("td-iso-theirs");
    seed_operations(&store, &mine, "td-iso-mine", 4, 0).await;
    seed_operations(&store, &theirs, "td-iso-theirs", 1, 0).await;

    let report = collect_queue_depth(&store, &mine, "projection")
        .await
        .expect("collect");
    assert_eq!(report.backlog(), 4, "the other tenant's row must not count");
}

#[tokio::test]
async fn firing_rules_open_idempotent_incidents() {
    let Some(store) = gated().await else {
        eprintln!("skip: {ENV} is not set (CI has no database)");
        return;
    };
    let tenant = unique_tenant("td-alert");
    seed_operations(&store, &tenant, "td-alert", 1, 1).await;
    let log = IncidentLog::new(Arc::new(store.clone()));

    let report = collect_queue_depth(&store, &tenant, "projection")
        .await
        .expect("collect");
    // Tiny thresholds: one overdue attempt fires, and a saturated
    // mailbox sample joins it.
    let thresholds = AlertThresholds {
        overdue_warn: 1,
        overdue_critical: 2,
        mailbox_warn_ratio: 0.8,
        ..AlertThresholds::default()
    };
    let mailboxes = vec![MailboxDepthSample {
        name: unique_entity("mbox", "td-alert"),
        occupancy: 95,
        capacity: 100,
    }];
    let fires = evaluate(&report, &mailboxes, &thresholds);
    assert!(
        fires.iter().any(|a| a.rule == "attempts-overdue"),
        "{fires:?}"
    );
    assert!(
        fires.iter().any(|a| a.rule == "mailbox-saturation"),
        "{fires:?}"
    );

    // Open them all; then re-run the whole pass — replays are no-ops.
    let mut first_ids = Vec::new();
    for alert in &fires {
        let opened = log
            .open(alert.draft(tenant.as_str()), 5_000)
            .await
            .expect("open incident");
        first_ids.push(alert.incident_id.clone());
        assert!(opened, "first evaluation must open {first_ids:?}");
    }
    for alert in &fires {
        let replayed = log
            .open(alert.draft(tenant.as_str()), 6_000)
            .await
            .expect("replay");
        assert!(!replayed, "replay of {} must be a no-op", alert.incident_id);
    }
    assert_eq!(first_ids.len(), fires.len());
}

#[tokio::test]
async fn a_quiet_tenant_raises_nothing() {
    let Some(store) = gated().await else {
        eprintln!("skip: {ENV} is not set (CI has no database)");
        return;
    };
    let tenant = unique_tenant("td-quiet");
    store
        .upsert_tenant(&tenant_record(&tenant, "td-quiet"))
        .await
        .expect("tenant insert");
    let report = collect_queue_depth(&store, &tenant, "projection")
        .await
        .expect("collect on an empty tenant");
    assert_eq!(report.backlog(), 0);
    assert_eq!(report.in_flight_attempts, 0);
    assert_eq!(report.overdue_attempts, 0);
    assert_eq!(report.consumer_lag, 0);
    assert!(evaluate(&report, &[], &AlertThresholds::default()).is_empty());
}
