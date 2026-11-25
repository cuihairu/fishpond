//! Mock roundtrip for every tool: one happy path per tool through the
//! full MCP JSON-RPC path (in-memory transport).

mod common;

use common::{call_error, call_json, spawn_mock};
use fishpond_mcp::mock::KEY_DEFAULT_TENANT;
use serde_json::json;

#[tokio::test]
async fn list_accounts_returns_tenant_accounts() {
    let client = spawn_mock(Some(KEY_DEFAULT_TENANT.to_owned())).await;
    let out = call_json(&client, "list_accounts", json!({})).await;
    let accounts = out["items"].as_array().expect("items array");
    assert_eq!(accounts.len(), 3);
    assert!(accounts.iter().all(|a| a["tenantId"] == "t-default"));
}

#[tokio::test]
async fn get_account_returns_state_machine_string() {
    let client = spawn_mock(Some(KEY_DEFAULT_TENANT.to_owned())).await;
    let out = call_json(&client, "get_account", json!({"id": "acc-100"})).await;
    assert_eq!(out["accountId"], "acc-100");
    assert_eq!(out["state"], "ONLINE");

    let unknown = call_json(&client, "get_account", json!({"id": "acc-102"})).await;
    assert_eq!(unknown["state"], "UNKNOWN");
    assert!(unknown["stateNote"].as_str().unwrap().contains("UNKNOWN"));
}

#[tokio::test]
async fn list_conversations_supports_account_filter() {
    let client = spawn_mock(Some(KEY_DEFAULT_TENANT.to_owned())).await;
    let out = call_json(
        &client,
        "list_conversations",
        json!({"accountId": "acc-100"}),
    )
    .await;
    let items = out["items"].as_array().expect("items array");
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["conversationId"], "conv-1");
}

#[tokio::test]
async fn list_messages_returns_conversation_history() {
    let client = spawn_mock(Some(KEY_DEFAULT_TENANT.to_owned())).await;
    let out = call_json(
        &client,
        "list_messages",
        json!({"conversationId": "conv-1"}),
    )
    .await;
    let items = out["items"].as_array().expect("items array");
    assert_eq!(items.len(), 2);
    assert_eq!(items[0]["messageId"], "m-1");
    assert_eq!(items[0]["direction"], "inbound");
    assert_eq!(items[1]["direction"], "outbound");
}

#[tokio::test]
async fn send_message_roundtrips_through_mock() {
    let client = spawn_mock(Some(KEY_DEFAULT_TENANT.to_owned())).await;
    let out = call_json(
        &client,
        "send_message",
        json!({
            "conversationId": "conv-1",
            "text": "mock roundtrip",
            "idempotencyKey": "rt-1"
        }),
    )
    .await;
    assert_eq!(out["message"]["conversationId"], "conv-1");
    assert_eq!(out["message"]["direction"], "outbound");
    assert_eq!(out["message"]["text"], "mock roundtrip");
    assert_eq!(out["deduplicated"], false);

    // The sent message shows up in the history.
    let history = call_json(
        &client,
        "list_messages",
        json!({"conversationId": "conv-1"}),
    )
    .await;
    let texts: Vec<&str> = history["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["text"].as_str().unwrap())
        .collect();
    assert!(texts.contains(&"mock roundtrip"));

    // Empty text is rejected.
    let err = call_error(
        &client,
        "send_message",
        json!({"conversationId": "conv-1", "text": "  ", "idempotencyKey": "rt-2"}),
    )
    .await;
    assert!(err.contains("invalid argument"), "got: {err}");
}

#[tokio::test]
async fn list_products_returns_catalog() {
    let client = spawn_mock(Some(KEY_DEFAULT_TENANT.to_owned())).await;
    let out = call_json(&client, "list_products", json!({"limit": 2})).await;
    let items = out["items"].as_array().expect("items array");
    assert_eq!(items.len(), 2);
    assert_eq!(out["nextCursor"], "2");
}

#[tokio::test]
async fn get_product_returns_fields() {
    let client = spawn_mock(Some(KEY_DEFAULT_TENANT.to_owned())).await;
    let out = call_json(&client, "get_product", json!({"id": "p-1"})).await;
    assert_eq!(out["productId"], "p-1");
    assert_eq!(out["priceCents"], 29_900);
    assert_eq!(out["currency"], "CNY");
}

#[tokio::test]
async fn list_operations_returns_operations() {
    let client = spawn_mock(Some(KEY_DEFAULT_TENANT.to_owned())).await;
    let out = call_json(&client, "list_operations", json!({})).await;
    let items = out["items"].as_array().expect("items array");
    assert_eq!(items.len(), 2);
    let kinds: Vec<&str> = items.iter().map(|o| o["kind"].as_str().unwrap()).collect();
    assert!(kinds.contains(&"send_message"));
}

#[tokio::test]
async fn get_operation_includes_attempt_summaries() {
    let client = spawn_mock(Some(KEY_DEFAULT_TENANT.to_owned())).await;
    let out = call_json(&client, "get_operation", json!({"id": "op-1"})).await;
    assert_eq!(out["operationId"], "op-1");
    let attempts = out["attempts"].as_array().expect("attempts array");
    assert_eq!(attempts.len(), 1);
    assert_eq!(attempts[0]["attemptId"], "a-1");
    assert_eq!(attempts[0]["outcome"], "succeeded");
}

#[tokio::test]
async fn subscribe_events_returns_snapshot() {
    let client = spawn_mock(Some(KEY_DEFAULT_TENANT.to_owned())).await;
    let out = call_json(&client, "subscribe_events", json!({"limit": 3})).await;
    let events = out.as_array().expect("events array");
    assert_eq!(events.len(), 3);
    assert!(events.iter().all(|e| e["eventType"].is_string()));
    // Snapshot is newest-last of the tenant log (seq 3,4,5).
    let seqs: Vec<u64> = events.iter().map(|e| e["seq"].as_u64().unwrap()).collect();
    assert_eq!(seqs, vec![3, 4, 5]);
}
