//! F017 acceptance (service side): publish, consume with dedup and gap
//! detection, gap sealing and log replay.
//!
//! The redelivery contract: an apply happens at most once per consumer
//! even when the transport delivers an event repeatedly; a batch that
//! skips past the checkpoint applies nothing and reports the missing
//! sequences; replay from the log (which has no holes) closes the gap.

use std::cell::RefCell;
use std::fmt;

use fishpond_domain::{EventId, EventPayload, TenantId, UnixMillis};
use fishpond_events::{ConsumeError, EventService};
use fishpond_storage::{Store, TenantRecord};

const ENV: &str = "FISHPOND_TEST_DATABASE_URL";

fn ts(millis: i64) -> UnixMillis {
    UnixMillis::from_millis(millis).expect("non-negative millis")
}

fn unique_token(tag: &str) -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock is after the epoch")
        .as_nanos();
    format!("{tag}-{}-{nanos}", std::process::id())
}

async fn gated_service() -> Option<(Store, EventService)> {
    let url = std::env::var(ENV).ok().filter(|v| !v.trim().is_empty())?;
    let store = Store::connect(&url).await.expect("connect + migrate");
    let service = EventService::new(store.clone());
    Some((store, service))
}

/// Counts apply invocations per event sequence across consume calls.
#[derive(Default)]
struct ApplyCount {
    seqs: RefCell<Vec<u64>>,
    fail_at: RefCell<Option<u64>>,
}

#[derive(Debug)]
struct ForcedFailure(u64);

impl fmt::Display for ForcedFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "forced failure at seq {}", self.0)
    }
}

impl std::error::Error for ForcedFailure {}

impl ApplyCount {
    /// The apply closure: records the sequence, optionally fails.
    fn closure(
        &self,
    ) -> impl FnMut(
        &fishpond_domain::EventEnvelope,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>>
    + '_ {
        |env: &fishpond_domain::EventEnvelope| {
            self.seqs.borrow_mut().push(env.seq);
            if self.fail_at.borrow().as_ref() == Some(&env.seq) {
                return Err(Box::new(ForcedFailure(env.seq)) as _);
            }
            Ok(())
        }
    }

    fn calls(&self, seq: u64) -> usize {
        self.seqs.borrow().iter().filter(|s| **s == seq).count()
    }
}

async fn tenant_for(store: &Store, tag: &str) -> TenantId {
    let tenant = TenantId::parse(unique_token(tag)).expect("tenant id is valid");
    store
        .upsert_tenant(&TenantRecord {
            id: tenant.clone(),
            name: tag.to_owned(),
            created_at: ts(1_000),
        })
        .await
        .expect("tenant insert");
    tenant
}

/// Publishes `count` events and returns their sealed envelopes.
async fn publish_batch(
    service: &EventService,
    tenant: &TenantId,
    tag: &str,
    count: u64,
) -> Vec<fishpond_domain::EventEnvelope> {
    // NewEvent takes references, so the owned parts live in this Vec.
    let mut events = Vec::with_capacity(count as usize);
    for i in 1..=count {
        let payload = EventPayload::parse(format!(r#"{{"n":{i}}}"#)).expect("payload");
        let id = EventId::parse(unique_token(&format!("{tag}-{i}"))).expect("event id");
        events.push((id, payload));
    }
    let refs: Vec<fishpond_storage::NewEvent<'_>> = events
        .iter()
        .map(|(id, payload)| fishpond_storage::NewEvent {
            id,
            event_type: "test.tick",
            account_id: None,
            occurred_at: ts(1_000),
            payload,
        })
        .collect();
    service.publish(tenant, &refs).await.expect("publish")
}

#[tokio::test]
async fn redelivery_applies_exactly_once() {
    let Some((_store, service)) = gated_service().await else {
        eprintln!("skip: {ENV} is not set (CI has no database)");
        return;
    };
    let tenant = tenant_for(service.store(), "svc-once").await;
    let sealed = publish_batch(&service, &tenant, "svc-once", 3).await;
    let counts = ApplyCount::default();

    let report = service
        .consume_batch(&tenant, "proj", &sealed, ts(2_000), counts.closure())
        .await
        .expect("first delivery");
    assert_eq!(report.applied.len(), 3);
    assert!(report.duplicates.is_empty());
    assert!(report.missing_seqs.is_empty());
    assert_eq!(report.checkpoint, 3);
    assert_eq!(counts.calls(1), 1);
    assert_eq!(counts.calls(3), 1);

    // The transport delivers the same batch again: everything sits at
    // or below the checkpoint, so it is certified without touching the
    // inbox — nothing re-applies, the checkpoint holds.
    let report = service
        .consume_batch(&tenant, "proj", &sealed, ts(3_000), counts.closure())
        .await
        .expect("redelivery");
    assert!(report.applied.is_empty());
    assert!(
        report.duplicates.is_empty(),
        "certified events skip the inbox entirely"
    );
    assert_eq!(report.checkpoint, 3);
    assert_eq!(
        counts.calls(1),
        1,
        "apply ran exactly once across deliveries"
    );
    assert_eq!(counts.calls(3), 1);

    // A different consumer has its own inbox and checkpoint; it builds
    // state from scratch by replaying the whole log.
    let report = service
        .catch_up(
            &tenant,
            "other-proj",
            0,
            ts(3_000),
            ApplyCount::default().closure(),
        )
        .await
        .expect("second consumer");
    assert_eq!(
        report.applied.len(),
        3,
        "the second consumer applies independently"
    );
    assert_eq!(report.checkpoint, 3);
    assert_eq!(service.checkpoint(&tenant, "proj").await.expect("ckpt"), 3);
    assert_eq!(
        service
            .checkpoint(&tenant, "other-proj")
            .await
            .expect("ckpt"),
        3
    );
}

#[tokio::test]
async fn late_batch_reports_a_gap_and_applies_nothing() {
    let Some((_store, service)) = gated_service().await else {
        eprintln!("skip: {ENV} is not set (CI has no database)");
        return;
    };
    let tenant = tenant_for(service.store(), "svc-gap").await;
    let sealed = publish_batch(&service, &tenant, "svc-gap", 4).await;
    let counts = ApplyCount::default();

    // The transport dropped events 1 and 2: the batch starts late.
    let report = service
        .consume_batch(&tenant, "proj", &sealed[2..], ts(2_000), counts.closure())
        .await
        .expect("late batch");
    assert_eq!(
        report.missing_seqs,
        vec![1, 2],
        "gap between checkpoint and batch"
    );
    assert!(report.applied.is_empty(), "nothing applies across a gap");
    assert!(report.duplicates.is_empty(), "no claims were taken either");
    assert_eq!(report.checkpoint, 0);
    assert_eq!(counts.seqs.borrow().len(), 0);

    // The first two arrive; the batch [1,2] applies and advances.
    let report = service
        .consume_batch(&tenant, "proj", &sealed[..2], ts(2_100), counts.closure())
        .await
        .expect("front half");
    assert!(report.missing_seqs.is_empty());
    assert_eq!(report.checkpoint, 2);
    assert_eq!(counts.calls(1), 1);

    // A hole opens inside the next batch (3 is still in transit).
    let report = service
        .consume_batch(&tenant, "proj", &sealed[3..], ts(2_200), counts.closure())
        .await
        .expect("holey batch");
    assert_eq!(
        report.missing_seqs,
        vec![3],
        "event 4 is delivered but 3 is not"
    );
    assert_eq!(report.checkpoint, 2);
    assert_eq!(
        counts.calls(4),
        0,
        "event 4 stays unapplied until 3 arrives"
    );

    // Replay from the log closes the gap: 3 applies, 4 dedups (its
    // claim from this batch was never taken — but the log redelivers
    // it, and the inbox has not seen it yet, so it applies now).
    let report = service
        .catch_up(&tenant, "proj", 0, ts(2_300), counts.closure())
        .await
        .expect("catch-up");
    assert!(report.missing_seqs.is_empty(), "the log has no holes");
    assert_eq!(report.applied.len(), 2, "events 3 and 4");
    assert_eq!(report.checkpoint, 4);
    assert_eq!(counts.calls(3), 1);
    assert_eq!(counts.calls(4), 1);
}

#[tokio::test]
async fn apply_failure_releases_the_claim_for_retry() {
    let Some((_store, service)) = gated_service().await else {
        eprintln!("skip: {ENV} is not set (CI has no database)");
        return;
    };
    let tenant = tenant_for(service.store(), "svc-fail").await;
    let sealed = publish_batch(&service, &tenant, "svc-fail", 2).await;
    let counts = ApplyCount {
        fail_at: RefCell::new(Some(2)),
        ..ApplyCount::default()
    };

    let err = service
        .consume_batch(&tenant, "proj", &sealed, ts(2_000), counts.closure())
        .await
        .expect_err("apply fails on event 2");
    match &err {
        ConsumeError::Apply { seq, event, .. } => {
            assert_eq!(*seq, 2);
            assert_eq!(*event, sealed[1].id);
        }
        other => panic!("expected an apply failure, got {other:?}"),
    }
    assert_eq!(counts.calls(1), 1, "event 1 applied before the failure");
    assert_eq!(
        service.checkpoint(&tenant, "proj").await.expect("ckpt"),
        0,
        "a failed batch does not advance the checkpoint"
    );

    // Retry with the failure cleared: event 1 dedups (its claim from
    // the successful apply stands), event 2 re-claims after the
    // release and applies.
    counts.fail_at.replace(None);
    let report = service
        .consume_batch(&tenant, "proj", &sealed, ts(2_100), counts.closure())
        .await
        .expect("retry");
    assert_eq!(report.applied.len(), 1, "only event 2 applies now");
    assert_eq!(report.duplicates.len(), 1, "event 1 was already applied");
    assert_eq!(report.checkpoint, 2);
    assert_eq!(counts.calls(1), 1, "event 1 never applied twice");
    assert_eq!(counts.calls(2), 2, "event 2: failed once, retried once");
}

#[tokio::test]
async fn gap_events_are_sealed_into_the_log() {
    let Some((_store, service)) = gated_service().await else {
        eprintln!("skip: {ENV} is not set (CI has no database)");
        return;
    };
    let tenant = tenant_for(service.store(), "svc-seal").await;
    let sealed = publish_batch(&service, &tenant, "svc-seal", 2).await;
    assert_eq!(sealed.last().expect("sealed").seq, 2);

    let gap_id = EventId::parse(unique_token("svc-seal-gap")).expect("event id");
    let gap = service
        .publish_gap(
            &tenant,
            &gap_id,
            &[1, 2],
            "webhook transport dropped",
            ts(3_000),
        )
        .await
        .expect("gap event");
    assert_eq!(gap.seq, 3, "the gap event takes the next sequence");
    assert_eq!(gap.event_type, "stream.gap");
    assert!(
        gap.payload.as_str().contains("\"missing\":[1,2]"),
        "payload documents the missing sequences: {}",
        gap.payload.as_str()
    );
    assert!(gap.payload.as_str().contains("webhook transport dropped"));

    let read_back = service
        .store()
        .get_event(&tenant, &gap_id)
        .await
        .expect("gap event roundtrip");
    assert_eq!(read_back, gap);
}
