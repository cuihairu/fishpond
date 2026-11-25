//! Idempotency-Key semantics of `send_message` through the MCP face.

mod common;

use common::{call_json, spawn_mock};
use fishpond_mcp::mock::KEY_DEFAULT_TENANT;
use serde_json::json;

#[tokio::test]
async fn same_idempotency_key_replays_original_message() {
    let client = spawn_mock(Some(KEY_DEFAULT_TENANT.to_owned())).await;

    let first = call_json(
        &client,
        "send_message",
        json!({"conversationId": "conv-1", "text": "first send", "idempotencyKey": "dup-1"}),
    )
    .await;
    assert_eq!(first["deduplicated"], false);
    let first_id = first["message"]["messageId"].as_str().unwrap().to_owned();

    // Same key, different body: the original message is replayed unchanged.
    let second = call_json(
        &client,
        "send_message",
        json!({"conversationId": "conv-1", "text": "different body", "idempotencyKey": "dup-1"}),
    )
    .await;
    assert_eq!(second["deduplicated"], true);
    assert_eq!(second["message"]["messageId"], first_id);
    assert_eq!(second["message"]["text"], "first send");

    // A different key creates a new message.
    let third = call_json(
        &client,
        "send_message",
        json!({"conversationId": "conv-1", "text": "first send", "idempotencyKey": "dup-2"}),
    )
    .await;
    assert_eq!(third["deduplicated"], false);
    assert_ne!(third["message"]["messageId"], first_id);

    // Exactly one message.sent event exists for the replayed send.
    let events = call_json(&client, "subscribe_events", json!({"limit": 100})).await;
    let sent: Vec<&serde_json::Value> = events
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["eventType"] == "message.sent")
        .collect();
    let with_id = sent
        .iter()
        .filter(|e| e["payload"]["messageId"] == first_id.as_str())
        .count();
    assert_eq!(with_id, 1, "replayed key must not append a second event");
}
