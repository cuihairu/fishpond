//! F019 acceptance: SSE session machinery — scope-gated opening,
//! bounded replay pages with persisted checkpoints, Last-Event-ID
//! reconnect, slow-consumer drop with a resume point, and tenant
//! scoping. The socket layer is F025's; this is the deterministic
//! service it drives.

use fishpond_domain::{EventId, EventPayload, TenantId, UnixMillis};
use fishpond_events::{EventService, SseStreams, StreamError, StreamPolicy};
use fishpond_storage::{
    NewEvent, ResolvedKey, SCOPE_EVENTS_READ, SCOPE_OPERATIONS_READ, Store, TenantRecord,
};

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

async fn gated() -> Option<(Store, EventService, SseStreams)> {
    let url = std::env::var(ENV).ok().filter(|v| !v.trim().is_empty())?;
    let store = Store::connect(&url).await.expect("connect + migrate");
    let events = EventService::new(store.clone());
    let streams = SseStreams::new(store.clone(), StreamPolicy::new(2, 100).expect("policy"));
    Some((store, events, streams))
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

fn key_for(tenant: &TenantId, scopes: &[&str]) -> ResolvedKey {
    ResolvedKey {
        tenant_id: tenant.clone(),
        role: "tester".to_owned(),
        scopes: scopes.iter().map(|s| (*s).to_owned()).collect(),
        expires_at: ts(9_999_999_999),
    }
}

/// Publishes `count` `test.tick` events, returning their envelopes.
async fn publish(
    events: &EventService,
    tenant: &TenantId,
    tag: &str,
    count: u64,
) -> Vec<fishpond_domain::EventEnvelope> {
    let mut owned = Vec::with_capacity(count as usize);
    for i in 1..=count {
        owned.push((
            EventId::parse(unique_token(&format!("{tag}-{i}"))).expect("event id"),
            EventPayload::parse(format!(r#"{{"i":{i}}}"#)).expect("payload"),
        ));
    }
    let batch: Vec<NewEvent<'_>> = owned
        .iter()
        .map(|(id, payload)| NewEvent {
            id,
            event_type: "test.tick",
            account_id: None,
            occurred_at: ts(1_500),
            payload,
        })
        .collect();
    events.publish(tenant, &batch).await.expect("publish")
}

#[tokio::test]
async fn stream_opening_requires_events_read_scope() {
    let Some((_store, _events, streams)) = gated().await else {
        eprintln!("skip: {ENV} is not set (CI has no database)");
        return;
    };
    let tenant = tenant_for(streams.store(), "sse19-scope").await;

    // A key without events:read is denied before anything else; the
    // error names the scope, nothing else.
    let bare = key_for(&tenant, &[SCOPE_OPERATIONS_READ]);
    let err = streams
        .open(&bare, "proj", None)
        .await
        .expect_err("scopeless key");
    match &err {
        StreamError::ScopeDenied { scope } => assert_eq!(*scope, SCOPE_EVENTS_READ),
        other => panic!("expected ScopeDenied, got {other:?}"),
    }

    // A scoped key opens; the session is bound to the key's tenant.
    let scoped = key_for(&tenant, &[SCOPE_EVENTS_READ]);
    let session = streams.open(&scoped, "proj", None).await.expect("open");
    assert_eq!(session.tenant(), &tenant);
    assert_eq!(session.consumer(), "proj");
    assert_eq!(session.cursor(), 0, "live tail on an empty log starts at 0");
}

#[tokio::test]
async fn pages_are_bounded_and_checkpointed() {
    let Some((_store, events, streams)) = gated().await else {
        eprintln!("skip: {ENV} is not set (CI has no database)");
        return;
    };
    let tenant = tenant_for(events.store(), "sse19-page").await;
    let sealed = publish(&events, &tenant, "page-evt", 5).await;
    assert_eq!(sealed.last().expect("last").seq, 5);

    let key = key_for(&tenant, &[SCOPE_EVENTS_READ]);
    // Replay is explicit: Last-Event-ID 0 walks the whole log.
    let mut session = streams.open(&key, "proj", Some(0)).await.expect("open");

    // Buffer of two: pages walk the log in bounded steps.
    let page = session.page(ts(2_000)).await.expect("page 1");
    assert_eq!(
        page.events.iter().map(|e| e.seq).collect::<Vec<_>>(),
        vec![1, 2]
    );
    assert_eq!(page.cursor, 2);
    assert!(!page.caught_up);

    let page = session.page(ts(2_100)).await.expect("page 2");
    assert_eq!(
        page.events.iter().map(|e| e.seq).collect::<Vec<_>>(),
        vec![3, 4]
    );

    let page = session.page(ts(2_200)).await.expect("page 3");
    assert_eq!(
        page.events.iter().map(|e| e.seq).collect::<Vec<_>>(),
        vec![5]
    );
    assert!(page.caught_up, "cursor reached the log head");

    let page = session.page(ts(2_300)).await.expect("page 4");
    assert!(page.events.is_empty());
    assert!(page.caught_up, "staying caught up");
    assert_eq!(session.cursor(), 5);

    // The checkpoint persisted: a fresh session for the same consumer
    // resumes where this one left off (no replay by default).
    let mut resumed = streams.open(&key, "proj", None).await.expect("reopen");
    let page = resumed.page(ts(2_400)).await.expect("page after reopen");
    assert!(page.events.is_empty(), "nothing new since seq 5");
    assert_eq!(resumed.cursor(), 5);

    // New events land and the existing session streams them.
    publish(&events, &tenant, "page-evt-late", 1).await;
    let page = session.page(ts(2_500)).await.expect("live page");
    assert_eq!(page.events.len(), 1);
    assert_eq!(page.events[0].seq, 6);
    assert!(page.caught_up);
}

#[tokio::test]
async fn reconnect_with_last_event_id_replays() {
    let Some((_store, events, streams)) = gated().await else {
        eprintln!("skip: {ENV} is not set (CI has no database)");
        return;
    };
    let tenant = tenant_for(events.store(), "sse19-replay").await;
    publish(&events, &tenant, "replay-evt", 4).await;

    // The client reconnects telling the server it stopped at seq 1:
    // the log is the truth, so replay resumes at 2.
    let key = key_for(&tenant, &[SCOPE_EVENTS_READ]);
    let mut session = streams
        .open(&key, "reconnectee", Some(1))
        .await
        .expect("reopen");
    let page = session.page(ts(2_000)).await.expect("page");
    assert_eq!(
        page.events.iter().map(|e| e.seq).collect::<Vec<_>>(),
        vec![2, 3],
        "bounded by the policy's buffer"
    );
    let page = session.page(ts(2_100)).await.expect("page 2");
    assert_eq!(
        page.events.iter().map(|e| e.seq).collect::<Vec<_>>(),
        vec![4]
    );
    assert!(page.caught_up);

    // A cursor below the stored checkpoint replays from there (the
    // client asks for it explicitly).
    let mut replays = streams
        .open(&key, "reconnectee", Some(0))
        .await
        .expect("replay open");
    let page = replays.page(ts(2_200)).await.expect("replay page");
    assert_eq!(
        page.events.iter().map(|e| e.seq).collect::<Vec<_>>(),
        vec![1, 2]
    );
}

#[tokio::test]
async fn slow_consumer_is_dropped_with_a_resume_point() {
    let Some((_store, events, _streams)) = gated().await else {
        eprintln!("skip: {ENV} is not set (CI has no database)");
        return;
    };
    let tenant = tenant_for(events.store(), "sse19-slow").await;
    publish(&events, &tenant, "slow-evt", 5).await;

    let tight = SseStreams::new(
        events.store().clone(),
        StreamPolicy::new(2, 3).expect("policy"),
    );
    let key = key_for(&tenant, &[SCOPE_EVENTS_READ]);

    // A live subscriber starts at the head, then stops paging while
    // five more events land: 5 undelivered sequences is past the
    // horizon of 3, so the session is dropped instead of buffered.
    let mut session = tight.open(&key, "slow", None).await.expect("open");
    assert_eq!(session.cursor(), 5, "live tail starts at the head");
    publish(&events, &tenant, "slow-evt-late", 5).await;
    let err = session.page(ts(2_000)).await.expect_err("slow consumer");
    match &err {
        StreamError::SlowConsumer { resume_from, lag } => {
            assert_eq!(*resume_from, 5, "the Last-Event-ID to reconnect with");
            assert_eq!(*lag, 5);
        }
        other => panic!("expected SlowConsumer, got {other:?}"),
    }

    // Nothing is lost by the drop: reconnect with Last-Event-ID 5 and
    // replay drains exactly the missed range — past the horizon and
    // all, since replay mode is exempt from the lag drop. (Before the
    // steady consumer below adds more to the log.)
    let mut resumed = tight.open(&key, "slow", Some(5)).await.expect("resume");
    let mut seen = Vec::new();
    for round in 0..3 {
        let page = resumed
            .page(ts(3_000 + round * 100))
            .await
            .expect("post-reconnect page");
        seen.extend(page.events.iter().map(|e| e.seq));
    }
    assert_eq!(seen, vec![6, 7, 8, 9, 10], "replay covers the whole gap");

    // A consumer that pages as events land stays within the horizon.
    let mut steady = tight.open(&key, "steady", None).await.expect("open");
    assert_eq!(steady.cursor(), 10, "live tail");
    let mut seen = Vec::new();
    for round in 0..2 {
        publish(&events, &tenant, &format!("steady-evt-{round}"), 2).await;
        let page = steady
            .page(ts(2_100 + round * 100))
            .await
            .expect("in-horizon page");
        seen.extend(page.events.iter().map(|e| e.seq));
    }
    assert_eq!(
        seen,
        vec![11, 12, 13, 14],
        "pages keep up with the publish rate"
    );
}

#[tokio::test]
async fn streams_never_cross_tenants() {
    let Some((_store, events, streams)) = gated().await else {
        eprintln!("skip: {ENV} is not set (CI has no database)");
        return;
    };
    let tenant_a = tenant_for(events.store(), "sse19-iso-a").await;
    let tenant_b = tenant_for(events.store(), "sse19-iso-b").await;

    // Both subscribe live while their logs are still empty.
    let key_a = key_for(&tenant_a, &[SCOPE_EVENTS_READ]);
    let key_b = key_for(&tenant_b, &[SCOPE_EVENTS_READ]);
    let mut session = streams.open(&key_a, "proj", None).await.expect("open");
    let mut session_b = streams.open(&key_b, "proj", None).await.expect("open");

    publish(&events, &tenant_b, "iso-b", 3).await;
    publish(&events, &tenant_a, "iso-a", 2).await;

    // The key's tenant binds the session: tenant A's log only.
    let page = session.page(ts(2_000)).await.expect("page");
    assert_eq!(
        page.events.iter().map(|e| e.seq).collect::<Vec<_>>(),
        vec![1, 2],
        "tenant A's sequences only"
    );
    assert!(page.events.iter().all(|e| e.tenant_id == tenant_a));

    // B's consumer sees B's stream, in B's own numbering (bounded by
    // the same buffer of two).
    let page = session_b.page(ts(2_000)).await.expect("page b 1");
    assert_eq!(
        page.events.iter().map(|e| e.seq).collect::<Vec<_>>(),
        vec![1, 2]
    );
    let page = session_b.page(ts(2_100)).await.expect("page b 2");
    assert_eq!(
        page.events.iter().map(|e| e.seq).collect::<Vec<_>>(),
        vec![3]
    );
    assert!(page.events.iter().all(|e| e.tenant_id == tenant_b));
}
