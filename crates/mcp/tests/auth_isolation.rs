//! Authorization contract: unauthenticated calls, missing scopes and
//! cross-tenant access are all rejected; secrets never leak into errors.

mod common;

use common::{call_error, call_json, spawn_mock};
use fishpond_mcp::mock::{KEY_DEFAULT_TENANT, KEY_OTHER_TENANT};
use serde_json::json;

#[tokio::test]
async fn missing_key_is_unauthenticated() {
    let client = spawn_mock(None).await;
    let err = call_error(&client, "list_accounts", json!({})).await;
    assert!(err.to_lowercase().contains("unauthenticated"), "got: {err}");
}

#[tokio::test]
async fn unknown_key_is_unauthenticated() {
    let client = spawn_mock(Some("fp_not_a_real_key_000000".to_owned())).await;
    let err = call_error(&client, "list_accounts", json!({})).await;
    assert!(err.to_lowercase().contains("unauthenticated"), "got: {err}");
    // The presented secret must not appear in the error message.
    assert!(!err.contains("fp_not_a_real_key_000000"));
}

#[tokio::test]
async fn missing_write_scope_denies_send() {
    // KEY_OTHER_TENANT is read-only: no conversations:write.
    let client = spawn_mock(Some(KEY_OTHER_TENANT.to_owned())).await;
    let err = call_error(
        &client,
        "send_message",
        json!({"conversationId": "conv-9", "text": "hi", "idempotencyKey": "k1"}),
    )
    .await;
    assert!(err.contains("permission denied"), "got: {err}");
    assert!(err.contains("conversations:write"), "got: {err}");
}

#[tokio::test]
async fn cross_tenant_resource_is_not_found() {
    // t-other key asking for a t-default account.
    let client = spawn_mock(Some(KEY_OTHER_TENANT.to_owned())).await;
    let err = call_error(&client, "get_account", json!({"id": "acc-100"})).await;
    assert!(err.contains("account not found"), "got: {err}");
    assert!(
        !err.contains("t-default"),
        "error must not leak the other tenant id: {err}"
    );

    // Same for products and operations.
    let err = call_error(&client, "get_product", json!({"id": "p-1"})).await;
    assert!(err.contains("product not found"), "got: {err}");
    let err = call_error(&client, "get_operation", json!({"id": "op-1"})).await;
    assert!(err.contains("operation not found"), "got: {err}");
}

#[tokio::test]
async fn other_tenant_sees_only_its_own_data() {
    let client = spawn_mock(Some(KEY_OTHER_TENANT.to_owned())).await;
    let out = call_json(&client, "list_accounts", json!({})).await;
    let items = out["items"].as_array().expect("items array");
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["accountId"], "acc-900");

    let own = call_json(&client, "get_account", json!({"id": "acc-900"})).await;
    assert_eq!(own["state"], "ONLINE");
}

#[tokio::test]
async fn default_tenant_read_scope_still_works() {
    // Sanity: the full-scope key passes the same paths that deny others.
    let client = spawn_mock(Some(KEY_DEFAULT_TENANT.to_owned())).await;
    let out = call_json(&client, "list_accounts", json!({})).await;
    assert_eq!(out["items"].as_array().unwrap().len(), 3);
}
