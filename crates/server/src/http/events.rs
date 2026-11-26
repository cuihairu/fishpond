//! Event-log routes (F025): the REST surface over the F017 event
//! store and the F019 stream sessions. Reads need `events:read`.
//!
//! `GET /events` is cursor pagination with the F019 cursor semantics:
//! the cursor is the last delivered sequence number and the next page
//! holds events strictly after it. `GET /events/stream` is the SSE
//! socket layer F019 deferred to this task: it opens a `StreamSession`
//! (live tail from head, `Last-Event-ID` replay, slow-consumer drop),
//! drains to the current head page by page, emits `id: <seq>` /
//! `event: <type>` / `data: <json>` frames plus a terminal
//! `caught-up` or `slow-consumer` event, then ends the connection —
//! the client reconnects (fresh subscribe = tail from the new head),
//! exactly as the F019 contract prescribes.

use std::collections::BTreeMap;
use std::convert::Infallible;

use axum::Json;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::sse::{Event as SseFrame, Sse};
use fishpond_domain::EventEnvelope;
use fishpond_events::{SseStreams, StreamError, StreamPolicy};
use fishpond_storage::ResolvedKey;
use tokio_stream::wrappers::ReceiverStream;

use super::AppState;
use super::accounts::require;
use super::auth::Caller;
use super::cursor;
use super::problem::Problem;

/// Per-page drain batch size (the session's policy buffer).
const STREAM_BUFFER: usize = 50;

/// Lag horizon before a live consumer is dropped (the F019 budget).
const STREAM_LAG_HORIZON: u64 = 500;

fn parse_after(raw: Option<&str>) -> Result<u64, Problem> {
    let Some(raw) = raw else {
        return Ok(0);
    };
    raw.parse::<u64>().map_err(|_| {
        Problem::new(
            "invalid-cursor",
            StatusCode::BAD_REQUEST,
            "invalid request",
            Some("cursor is not a valid sequence cursor".to_owned()),
        )
    })
}

fn event_json(event: &EventEnvelope) -> serde_json::Value {
    serde_json::json!({
        "id": event.id.as_str(),
        "seq": event.seq,
        "event_type": event.event_type,
        "account_id": event
            .account_id
            .as_ref()
            .map(fishpond_domain::AccountId::as_str),
        "occurred_at": event.occurred_at.as_millis(),
        "payload": serde_json::from_str::<serde_json::Value>(event.payload.as_str())
            .unwrap_or(serde_json::Value::Null),
    })
}

/// Adapts the REST caller (server-configured key) to the F007 key
/// shape the stream gate expects. The expiry is nominal: REST keys
/// live in process config and have no lifetime of their own.
fn resolved_key(caller: &Caller, now: fishpond_domain::UnixMillis) -> ResolvedKey {
    ResolvedKey {
        tenant_id: caller.tenant.clone(),
        role: "rest".to_owned(),
        scopes: caller.scopes.clone(),
        expires_at: now,
    }
}

/// `GET /events` — one bounded page strictly after `cursor` (the last
/// delivered seq; 0 or absent = from the beginning), ordered by seq.
/// `next_cursor` is set when the page filled, meaning the log may
/// continue past it.
pub async fn list(
    State(state): State<AppState>,
    caller: Caller,
    Query(params): Query<BTreeMap<String, String>>,
) -> Result<Json<serde_json::Value>, Problem> {
    require(&caller, "events:read")?;
    let limit = cursor::parse_limit(params.get("limit").map(String::as_str))?;
    let after = parse_after(params.get("cursor").map(String::as_str))?;
    let events = state
        .store
        .list_events(&caller.tenant, after, limit as u32)
        .await
        .map_err(|err| Problem::from(&err))?;
    let items: Vec<serde_json::Value> = events.iter().map(event_json).collect();
    let next_cursor = if events.len() == limit {
        events.last().map(|last| last.seq.to_string())
    } else {
        None
    };
    Ok(Json(serde_json::json!({
        "events": items,
        "next_cursor": next_cursor,
    })))
}

/// `GET /events/stream` — the SSE drain. One connection replays
/// (`Last-Event-ID`) or live-tails (from head) until the log head,
/// then ends with a `caught-up` frame; a consumer too far behind gets
/// `slow-consumer` with its `resume_from` instead. Clients reconnect.
pub async fn stream(
    State(state): State<AppState>,
    caller: Caller,
    headers: HeaderMap,
    Query(params): Query<BTreeMap<String, String>>,
) -> Result<Sse<ReceiverStream<Result<SseFrame, Infallible>>>, Problem> {
    require(&caller, "events:read")?;
    let last_event_id = headers
        .get("last-event-id")
        .and_then(|value| value.to_str().ok())
        .and_then(|raw| raw.parse::<u64>().ok());
    let consumer = params
        .get("consumer")
        .cloned()
        .unwrap_or_else(|| "rest-sse".to_owned());
    let policy =
        StreamPolicy::new(STREAM_BUFFER, STREAM_LAG_HORIZON).map_err(|err| Problem::from(&err))?;
    let now = super::now();
    let streams = SseStreams::new(state.store.clone(), policy);
    let mut session = streams
        .open(&resolved_key(&caller, now), &consumer, last_event_id)
        .await
        .map_err(map_stream_error)?;

    let (tx, rx) = tokio::sync::mpsc::channel::<Result<SseFrame, Infallible>>(8);
    tokio::spawn(async move {
        loop {
            match session.page(super::now()).await {
                Ok(page) => {
                    for event in &page.events {
                        let frame = SseFrame::default()
                            .event(event.event_type.clone())
                            .id(event.seq.to_string())
                            .data(event_json(event).to_string());
                        if tx.send(Ok(frame)).await.is_err() {
                            return; // client disconnected
                        }
                    }
                    if page.caught_up {
                        let _ = tx
                            .send(Ok(SseFrame::default().event("caught-up").data("")))
                            .await;
                        return;
                    }
                }
                Err(StreamError::SlowConsumer { resume_from, lag }) => {
                    let body = serde_json::json!({
                        "resume_from": resume_from,
                        "lag": lag,
                    });
                    let _ = tx
                        .send(Ok(SseFrame::default()
                            .event("slow-consumer")
                            .data(body.to_string())))
                        .await;
                    return;
                }
                Err(err) => {
                    let _ = tx
                        .send(Ok(SseFrame::default().event("error").data(err.to_string())))
                        .await;
                    return;
                }
            }
        }
    });
    Ok(Sse::new(ReceiverStream::new(rx)))
}

/// Stream failures keep their F019 classes: scope denied stays a 403
/// problem, storage stays a generic 500.
fn map_stream_error(err: StreamError) -> Problem {
    match err {
        StreamError::ScopeDenied { scope } => Problem::new(
            "scope-denied",
            StatusCode::FORBIDDEN,
            "scope denied",
            Some(format!("this call requires the {scope} scope")),
        ),
        StreamError::Storage(storage) => Problem::from(&storage),
        other => Problem::new(
            "invalid",
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid request",
            Some(other.to_string()),
        ),
    }
}
