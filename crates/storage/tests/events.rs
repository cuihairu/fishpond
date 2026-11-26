//! F017 acceptance (storage side): the tenant event log, consumer inbox
//! and projection checkpoints.
//!
//! The log must sequence per tenant without gaps even across separate
//! appends, dedup on event id (idempotent re-append returns the stored
//! envelope), and stay tenant-isolated. The inbox claim is the
//! at-most-once apply gate for at-least-once delivery; checkpoints
//! never move backwards.

mod common;

use common::{ENV, gated, tenant_record, ts, unique_entity, unique_tenant};
use fishpond_domain::{EventId, EventPayload};
use fishpond_storage::{InboxClaim, InboxDecision, NewEvent, Store};
use sqlx::Row;

fn payload(json: &str) -> EventPayload {
    EventPayload::parse(json).expect("payload is a JSON object")
}

fn event<'a>(id: &'a EventId, kind: &'a str, at: i64, body: &'a EventPayload) -> NewEvent<'a> {
    NewEvent {
        id,
        event_type: kind,
        account_id: None,
        occurred_at: ts(at),
        payload: body,
    }
}

async fn event_id(tag: &str) -> EventId {
    EventId::parse(unique_entity("evt", tag)).expect("event id is valid")
}

/// Sets up a fresh tenant row (events reference tenants).
async fn tenant_for(store: &Store, tag: &str) -> fishpond_domain::TenantId {
    let tenant = unique_tenant(tag);
    store
        .upsert_tenant(&tenant_record(&tenant, tag))
        .await
        .expect("tenant insert");
    tenant
}

#[tokio::test]
async fn log_sequences_gapless_across_batches_and_tenants() {
    let Some(store) = gated().await else {
        eprintln!("skip: {ENV} is not set (CI has no database)");
        return;
    };
    let tenant = tenant_for(&store, "evt-seq-a").await;
    let other = tenant_for(&store, "evt-seq-b").await;

    // First batch of three.
    let ids: Vec<EventId> =
        futures_join(3, |i| async move { event_id(&format!("seq-a{i}")).await }).await;
    let bodies: Vec<EventPayload> = ["{\"n\":1}", "{\"n\":2}", "{\"n\":3}"]
        .iter()
        .map(|raw| payload(raw))
        .collect();
    let batch: Vec<NewEvent<'_>> = ids
        .iter()
        .zip(&bodies)
        .enumerate()
        .map(|(i, (id, body))| event(id, "test.tick", 1_000 + i as i64, body))
        .collect();
    let sealed = store.append_events(&tenant, &batch).await.expect("append");
    assert_eq!(sealed.len(), 3);
    for (i, envelope) in sealed.iter().enumerate() {
        assert_eq!(envelope.seq, (i + 1) as u64, "sequences start at 1");
        assert_eq!(envelope.tenant_id, tenant);
        assert_eq!(envelope.event_type, "test.tick");
    }

    // A second batch continues the sequence without a gap.
    let late = event_id("seq-a-late").await;
    let sealed = store
        .append_events(
            &tenant,
            &[event(&late, "test.tick", 9_000, &payload("{\"n\":4}"))],
        )
        .await
        .expect("append");
    assert_eq!(sealed[0].seq, 4, "continues after the first batch");
    assert_eq!(store.latest_seq(&tenant).await.expect("latest"), 4);

    // Another tenant counts from its own 1.
    let other_id = event_id("seq-b").await;
    let sealed = store
        .append_events(
            &other,
            &[event(&other_id, "test.tick", 5_000, &payload("{}"))],
        )
        .await
        .expect("append");
    assert_eq!(sealed[0].seq, 1, "sequences are per tenant");
    assert_eq!(store.latest_seq(&other).await.expect("latest"), 1);
}

/// Runs `count` futures concurrently (models concurrent publishers).
async fn futures_join<F, Fut>(count: usize, make: F) -> Vec<Fut::Output>
where
    F: Fn(usize) -> Fut,
    Fut: Future,
{
    let runs: Vec<Fut> = (0..count).map(make).collect();
    futures::future::join_all(runs).await
}

#[tokio::test]
async fn concurrent_publishers_never_open_a_gap() {
    let Some(store) = gated().await else {
        eprintln!("skip: {ENV} is not set (CI has no database)");
        return;
    };
    let tenant = tenant_for(&store, "evt-race").await;
    let ids: Vec<EventId> =
        futures_join(6, |i| async move { event_id(&format!("race{i}")).await }).await;

    // Six publishers append one event each, concurrently; each batches a
    // single event through its own transaction.
    let appends = ids.iter().map(|id| {
        let tenant = tenant.clone();
        let id = id.clone();
        let body = payload("{\"r\":true}");
        let store = &store;
        async move {
            store
                .append_events(&tenant, &[event(&id, "test.race", 1_000, &body)])
                .await
                .expect("concurrent append")
        }
    });
    for sealed in futures::future::join_all(appends).await {
        assert_eq!(sealed.len(), 1);
    }

    // The log is 1..=6 with no hole.
    let log = store.list_events(&tenant, 0, 0).await.expect("log");
    let seqs: Vec<u64> = log.iter().map(|e| e.seq).collect();
    assert_eq!(seqs, vec![1, 2, 3, 4, 5, 6], "gapless under concurrency");
}

#[tokio::test]
async fn duplicate_event_id_is_an_idempotent_append() {
    let Some(store) = gated().await else {
        eprintln!("skip: {ENV} is not set (CI has no database)");
        return;
    };
    let tenant = tenant_for(&store, "evt-dup").await;
    let first = event_id("dup-1").await;
    let body = payload("{\"k\":\"v\"}");
    let sealed = store
        .append_events(&tenant, &[event(&first, "test.once", 1_000, &body)])
        .await
        .expect("first append");
    assert_eq!(sealed[0].seq, 1);

    // Re-delivering the same event id returns the stored envelope (same
    // sequence), and a fresh sibling in the same batch still gets the
    // next sequence — no hole, no duplicate row.
    let second = event_id("dup-2").await;
    let sealed = store
        .append_events(
            &tenant,
            &[
                event(&first, "test.once", 1_000, &body),
                event(&second, "test.once", 2_000, &payload("{\"k\":\"w\"}")),
            ],
        )
        .await
        .expect("re-append");
    assert_eq!(sealed.len(), 2);
    assert_eq!(sealed[0].id, first);
    assert_eq!(
        sealed[0].seq, 1,
        "stored sequence is returned, not a new one"
    );
    assert_eq!(sealed[1].seq, 2, "the fresh event takes the next sequence");

    let log = store.list_events(&tenant, 0, 0).await.expect("log");
    assert_eq!(log.len(), 2, "no duplicate row was written");
    assert_eq!(store.latest_seq(&tenant).await.expect("latest"), 2);
}

#[tokio::test]
async fn append_rejects_blank_event_types_and_empty_batches_are_fine() {
    let Some(store) = gated().await else {
        eprintln!("skip: {ENV} is not set (CI has no database)");
        return;
    };
    let tenant = tenant_for(&store, "evt-blank").await;
    let sealed = store
        .append_events(&tenant, &[])
        .await
        .expect("empty batch");
    assert!(sealed.is_empty());

    let id = event_id("blank-1").await;
    let body = payload("{}");
    let err = store
        .append_events(&tenant, &[event(&id, "   ", 1_000, &body)])
        .await
        .expect_err("blank event type");
    assert!(err.to_string().contains("event type"), "{err}");
    assert_eq!(
        store.latest_seq(&tenant).await.expect("latest"),
        0,
        "nothing was written"
    );
}

#[tokio::test]
async fn list_events_reads_after_a_cursor_with_a_limit() {
    let Some(store) = gated().await else {
        eprintln!("skip: {ENV} is not set (CI has no database)");
        return;
    };
    let tenant = tenant_for(&store, "evt-list").await;
    let ids: Vec<EventId> =
        futures_join(5, |i| async move { event_id(&format!("list{i}")).await }).await;
    let body = payload("{}");
    let batch: Vec<NewEvent<'_>> = ids
        .iter()
        .enumerate()
        .map(|(i, id)| event(id, "test.list", 1_000 + i as i64, &body))
        .collect();
    store.append_events(&tenant, &batch).await.expect("append");

    let page = store.list_events(&tenant, 2, 2).await.expect("page");
    assert_eq!(page.iter().map(|e| e.seq).collect::<Vec<_>>(), vec![3, 4]);

    let rest = store.list_events(&tenant, 4, 0).await.expect("rest");
    assert_eq!(rest.iter().map(|e| e.seq).collect::<Vec<_>>(), vec![5]);

    let one = store.get_event(&tenant, &ids[2]).await.expect("event");
    assert_eq!(one.seq, 3);
    let missing = event_id("list-missing").await;
    let err = store
        .get_event(&tenant, &missing)
        .await
        .expect_err("missing event");
    assert!(err.to_string().contains("event"), "{err}");
}

#[tokio::test]
async fn inbox_claim_dedups_and_release_reopens() {
    let Some(store) = gated().await else {
        eprintln!("skip: {ENV} is not set (CI has no database)");
        return;
    };
    let tenant = tenant_for(&store, "evt-inbox").await;
    let id = event_id("inbox-1").await;
    let claim = |consumer: &'static str| InboxClaim {
        consumer,
        event_id: &id,
        seq: 1,
        at: ts(1_000),
    };

    let decision = store
        .inbox_claim(&tenant, &claim("projector-a"))
        .await
        .expect("first claim");
    assert_eq!(decision, InboxDecision::Claimed, "first delivery applies");
    let decision = store
        .inbox_claim(&tenant, &claim("projector-a"))
        .await
        .expect("redelivery");
    assert_eq!(decision, InboxDecision::Duplicate, "redelivery dedups");
    let decision = store
        .inbox_claim(&tenant, &claim("projector-b"))
        .await
        .expect("other consumer");
    assert_eq!(decision, InboxDecision::Claimed, "dedup is per consumer");

    // A failed apply releases the claim so the retry can proceed.
    store
        .inbox_release(&tenant, "projector-b", &id)
        .await
        .expect("release");
    let decision = store
        .inbox_claim(&tenant, &claim("projector-b"))
        .await
        .expect("retry after release");
    assert_eq!(
        decision,
        InboxDecision::Claimed,
        "released claim is re-claimable"
    );
}

#[tokio::test]
async fn checkpoints_never_move_backwards() {
    let Some(store) = gated().await else {
        eprintln!("skip: {ENV} is not set (CI has no database)");
        return;
    };
    let tenant = tenant_for(&store, "evt-ckpt").await;

    assert_eq!(store.checkpoint(&tenant, "proj").await.expect("ckpt"), 0);
    store
        .advance_checkpoint(&tenant, "proj", 5, ts(1_000))
        .await
        .expect("advance");
    // A stale consumer reporting an older cursor must not rewind.
    store
        .advance_checkpoint(&tenant, "proj", 3, ts(2_000))
        .await
        .expect("stale advance");
    assert_eq!(store.checkpoint(&tenant, "proj").await.expect("ckpt"), 5);
    store
        .advance_checkpoint(&tenant, "proj", 7, ts(3_000))
        .await
        .expect("advance");
    assert_eq!(store.checkpoint(&tenant, "proj").await.expect("ckpt"), 7);
    // Checkpoints are per consumer.
    assert_eq!(store.checkpoint(&tenant, "other").await.expect("ckpt"), 0);
}

#[tokio::test]
async fn event_surfaces_are_tenant_isolated() {
    let Some(store) = gated().await else {
        eprintln!("skip: {ENV} is not set (CI has no database)");
        return;
    };
    let tenant = tenant_for(&store, "evt-iso-a").await;
    let other = tenant_for(&store, "evt-iso-b").await;

    let id = event_id("iso-1").await;
    store
        .append_events(
            &tenant,
            &[event(&id, "test.iso", 1_000, &payload("{\"t\":\"a\"}"))],
        )
        .await
        .expect("append");
    store
        .inbox_claim(
            &tenant,
            &InboxClaim {
                consumer: "proj",
                event_id: &id,
                seq: 1,
                at: ts(1_000),
            },
        )
        .await
        .expect("claim");
    store
        .advance_checkpoint(&tenant, "proj", 1, ts(1_000))
        .await
        .expect("checkpoint");

    // The bare pool sees nothing in any of the three tables.
    for table in ["events", "event_inbox", "projection_checkpoints"] {
        let bare: i64 = sqlx::query(&format!("SELECT count(*) FROM {table}"))
            .fetch_one(store.pool())
            .await
            .expect("bare count")
            .try_get("count")
            .expect("count column");
        assert_eq!(bare, 0, "{table} leaks no tenant rows to the bare pool");
    }

    // A foreign tenant reads an empty log and a zero checkpoint.
    assert!(
        store
            .list_events(&other, 0, 0)
            .await
            .expect("log")
            .is_empty()
    );
    assert_eq!(store.checkpoint(&other, "proj").await.expect("ckpt"), 0);
    let err = store
        .get_event(&other, &id)
        .await
        .expect_err("cross-tenant read");
    assert!(err.to_string().contains("event"), "{err}");

    // The owner sees its own row.
    assert_eq!(
        store.list_events(&tenant, 0, 0).await.expect("log").len(),
        1
    );
}
