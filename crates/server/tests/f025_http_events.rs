//! F025 acceptance (event log): the `GET /events` cursor contract and
//! the `GET /events/stream` SSE drain over the real event store —
//! seq cursors, scope enforcement, tenant isolation, and the
//! caught-up / replay semantics F019 promised. Skips without a
//! database, like every other DB-backed suite.

#[allow(dead_code)]
mod common;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::{gated, tenant_record, ts, unique_entity, unique_tenant};
use fishpond_domain::{AccountId, EventId, EventPayload, TenantId};
use fishpond_server::http::auth::{RestKeyRecord, RestKeys};
use fishpond_server::{AppState, build_router};
use fishpond_storage::{NewEvent, Store};
use http_body_util::BodyExt;
use tower::ServiceExt;

/// Keys: one full-scope (includes `events:read`) and one read-only
/// account key (deliberately without it).
async fn setup(tag: &str) -> (Router, TenantId, Store) {
    let store = gated().await.expect("checked by caller");
    let tenant = unique_tenant(tag);
    store
        .upsert_tenant(&tenant_record(&tenant, tag))
        .await
        .expect("tenant insert");
    let mut keys = RestKeys::new();
    keys.insert(
        format!("key-{tag}"),
        RestKeyRecord {
            tenant_id: tenant.clone(),
            scopes: vec![
                "accounts:read".to_owned(),
                "events:read".to_owned(),
                "sessions:read".to_owned(),
            ],
        },
    );
    keys.insert(
        format!("key-ro-{tag}"),
        RestKeyRecord {
            tenant_id: tenant.clone(),
            scopes: vec!["accounts:read".to_owned()],
        },
    );
    let state = AppState::new(store.clone(), keys);
    (build_router(state), tenant, store)
}

async fn call(
    router: &Router,
    method: &str,
    uri: &str,
    key: Option<&str>,
    headers: &[(&str, &str)],
) -> (StatusCode, String) {
    let mut builder = Request::builder().method(method).uri(uri);
    if let Some(key) = key {
        builder = builder.header("authorization", format!("Bearer {key}"));
    }
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    let request = builder.body(Body::empty()).expect("request");
    let response = router.clone().oneshot(request).await.expect("response");
    let status = response.status();
    let content_type = response
        .headers()
        .get("content-type")
        .map(|value| value.to_str().expect("ascii").to_owned());
    let text = String::from_utf8(
        BodyExt::collect(response.into_body())
            .await
            .expect("body")
            .to_bytes()
            .to_vec(),
    )
    .expect("utf-8");
    if let Some(content_type) = content_type {
        // Problem documents are problem+json; streams are SSE.
        assert!(
            content_type.contains("json") || content_type.starts_with("text/event-stream"),
            "{content_type}"
        );
    }
    (status, text)
}

/// Appends `count` events to `tenant`'s log.
async fn append(store: &Store, tenant: &TenantId, tag: &str, count: usize) {
    let ids: Vec<EventId> = (0..count)
        .map(|i| EventId::parse(unique_entity("evt", &format!("{tag}{i}"))).expect("event id"))
        .collect();
    let payloads: Vec<EventPayload> = (0..count)
        .map(|i| EventPayload::parse(format!(r#"{{"i":{i}}}"#)).expect("payload"))
        .collect();
    let rows: Vec<NewEvent<'_>> = ids
        .iter()
        .zip(payloads.iter())
        .map(|(id, payload)| NewEvent {
            id,
            event_type: "test.pinged",
            account_id: None,
            occurred_at: ts(3_000),
            payload,
        })
        .collect();
    store
        .append_events(tenant, &rows)
        .await
        .expect("events append");
}

#[tokio::test]
async fn events_list_cursor_walk_scopes_and_problems() {
    if gated().await.is_none() {
        eprintln!("skip: no database");
        return;
    }
    let (router, tenant, store) = setup("f025-evt").await;
    let key = "key-f025-evt";
    append(&store, &tenant, "evt", 5).await;

    // Page 1: limit 2 → seqs 1,2 and a follow-up cursor.
    let (status, page1) = call(&router, "GET", "/events?limit=2", Some(key), &[]).await;
    assert_eq!(status, StatusCode::OK, "{page1}");
    let doc: serde_json::Value = serde_json::from_str(&page1).expect("json");
    let seqs: Vec<u64> = doc["events"]
        .as_array()
        .expect("events")
        .iter()
        .map(|event| event["seq"].as_u64().expect("seq"))
        .collect();
    assert_eq!(seqs, vec![1, 2], "strictly ordered from the start");
    assert_eq!(doc["events"][0]["event_type"], "test.pinged");
    assert_eq!(
        doc["events"][0]["payload"]["i"], 0,
        "payload arrives as JSON"
    );
    assert_eq!(doc["next_cursor"], "2");

    // The cursor is the last delivered seq; the next page is strictly
    // after it.
    let (status, page2) = call(&router, "GET", "/events?limit=2&cursor=2", Some(key), &[]).await;
    assert_eq!(status, StatusCode::OK, "{page2}");
    let doc: serde_json::Value = serde_json::from_str(&page2).expect("json");
    let seqs: Vec<u64> = doc["events"]
        .as_array()
        .expect("events")
        .iter()
        .map(|event| event["seq"].as_u64().expect("seq"))
        .collect();
    assert_eq!(seqs, vec![3, 4]);
    assert_eq!(doc["next_cursor"], "4");

    // The tail page: one event, no follow-up cursor.
    let (status, page3) = call(&router, "GET", "/events?cursor=4", Some(key), &[]).await;
    assert_eq!(status, StatusCode::OK, "{page3}");
    let doc: serde_json::Value = serde_json::from_str(&page3).expect("json");
    assert_eq!(doc["events"].as_array().expect("events").len(), 1);
    assert_eq!(doc["next_cursor"], serde_json::Value::Null);

    // A mangled cursor is a 400 problem.
    let (status, problem) = call(&router, "GET", "/events?cursor=banana", Some(key), &[]).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{problem}");
    assert!(
        problem.contains("invalid-cursor"),
        "problem names the class: {problem}"
    );

    // Scope enforcement: accounts:read does not carry events:read.
    let (status, problem) = call(&router, "GET", "/events", Some("key-ro-f025-evt"), &[]).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{problem}");
    assert!(problem.contains("events:read"), "{problem}");
    let (status, problem) = call(&router, "GET", "/events", None, &[]).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{problem}");

    // Tenant isolation: another tenant's key sees an empty log.
    let other = unique_tenant("f025-evt-other");
    store
        .upsert_tenant(&tenant_record(&other, "f025-evt-other"))
        .await
        .expect("other tenant");
    let mut keys = RestKeys::new();
    keys.insert(
        "key-evt-other",
        RestKeyRecord {
            tenant_id: other,
            scopes: vec!["events:read".to_owned()],
        },
    );
    let isolated = build_router(AppState::new(store.clone(), keys));
    let (status, page) = call(&isolated, "GET", "/events", Some("key-evt-other"), &[]).await;
    assert_eq!(status, StatusCode::OK, "{page}");
    let doc: serde_json::Value = serde_json::from_str(&page).expect("json");
    assert!(doc["events"].as_array().expect("events").is_empty());
}

#[tokio::test]
async fn events_stream_drains_replays_and_checkpoints() {
    if gated().await.is_none() {
        eprintln!("skip: no database");
        return;
    }
    let (router, tenant, store) = setup("f025-sse").await;
    let key = "key-f025-sse";
    append(&store, &tenant, "sse", 3).await;

    // A fresh subscribe live-tails from the head (the F019 contract:
    // "live tail starts at the head") — no backlog, just the
    // terminal caught-up frame.
    let (status, stream) = call(
        &router,
        "GET",
        "/events/stream?consumer=drain",
        Some(key),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{stream}");
    assert!(
        !stream.contains("id: 1\n") && stream.contains("event: caught-up"),
        "live tail starts at the head, no backlog: {stream}"
    );

    // The backlog is a client-authoritative replay: an explicit
    // Last-Event-ID drains it, then the caught-up terminal.
    let (status, stream) = call(
        &router,
        "GET",
        "/events/stream?consumer=drain",
        Some(key),
        &[("last-event-id", "0")],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{stream}");
    for seq in 1..=3 {
        assert!(stream.contains(&format!("id: {seq}\n")), "{stream}");
    }
    assert!(stream.contains("event: test.pinged"), "{stream}");
    assert!(
        stream.contains(r#""i":1"#),
        "payload json is the frame body"
    );
    assert!(stream.contains("event: caught-up"), "{stream}");

    // The replay persisted the checkpoint: a plain reconnect starts
    // at the head — nothing replayed again.
    let (status, stream) = call(
        &router,
        "GET",
        "/events/stream?consumer=drain",
        Some(key),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{stream}");
    assert!(
        !stream.contains("id: 1\n"),
        "checkpoint resumes at head: {stream}"
    );
    assert!(stream.contains("event: caught-up"), "{stream}");

    // Reconnect with Last-Event-ID: 1 replays strictly after seq 1.
    let (status, stream) = call(
        &router,
        "GET",
        "/events/stream?consumer=drain",
        Some(key),
        &[("last-event-id", "1")],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{stream}");
    assert!(
        stream.contains("id: 2\n") && stream.contains("id: 3\n"),
        "{stream}"
    );
    assert!(
        !stream.contains("id: 1\n"),
        "replay starts after the id: {stream}"
    );

    // Scope enforcement: no events:read → the stream gate rejects
    // with the same 403 problem class.
    let (status, problem) = call(
        &router,
        "GET",
        "/events/stream",
        Some("key-ro-f025-sse"),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{problem}");
    assert!(problem.contains("events:read"), "{problem}");

    // A garbage Last-Event-ID is ignored (live tail), not a 500.
    let (status, stream) = call(
        &router,
        "GET",
        "/events/stream?consumer=drain",
        Some(key),
        &[("last-event-id", "potato")],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{stream}");
    assert!(stream.contains("event: caught-up"), "{stream}");
}

// `account_id` stays exercised via the type: an event bound to an
// account round-trips the id in the DTO.
#[tokio::test]
async fn events_list_carries_account_ids() {
    if gated().await.is_none() {
        eprintln!("skip: no database");
        return;
    }
    let (router, tenant, store) = setup("f025-evt-acc").await;
    let key = "key-f025-evt-acc";
    let account_id = AccountId::parse(unique_entity("acc", "evtacc")).expect("account id");
    let id = EventId::parse(unique_entity("evt", "accbound")).expect("event id");
    let payload = EventPayload::parse(r#"{"bound":true}"#).expect("payload");
    store
        .append_events(
            &tenant,
            &[NewEvent {
                id: &id,
                event_type: "account.noticed",
                account_id: Some(&account_id),
                occurred_at: ts(4_000),
                payload: &payload,
            }],
        )
        .await
        .expect("append");
    let (status, page) = call(&router, "GET", "/events", Some(key), &[]).await;
    assert_eq!(status, StatusCode::OK, "{page}");
    assert!(
        page.contains(account_id.as_str()),
        "account id surfaces: {page}"
    );
}
