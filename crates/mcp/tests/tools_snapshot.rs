//! Contract test: the tools/list surface matches the committed golden
//! snapshot (names, order, descriptions, input schemas).
//!
//! Regenerate with `UPDATE_SNAPSHOT=1 cargo test -p fishpond-mcp`.

mod common;

use common::spawn_mock;
use fishpond_mcp::mock::KEY_DEFAULT_TENANT;

const SNAPSHOT_PATH: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/snapshot/tools_list.json"
);

// rmcp sorts tools/list output by tool name before sending it, so the
// advertised order is deterministic; the contract pins that exact order.
const EXPECTED_TOOLS: [&str; 10] = [
    "get_account",
    "get_operation",
    "get_product",
    "list_accounts",
    "list_conversations",
    "list_messages",
    "list_operations",
    "list_products",
    "send_message",
    "subscribe_events",
];

#[tokio::test]
async fn tools_list_matches_golden_snapshot() {
    let client = spawn_mock(Some(KEY_DEFAULT_TENANT.to_owned())).await;
    let tools = client.list_all_tools().await.expect("tools/list succeeds");

    let names: Vec<String> = tools.iter().map(|t| t.name.to_string()).collect();
    assert_eq!(
        names, EXPECTED_TOOLS,
        "tool set or order drifted from the contract"
    );

    let value = serde_json::to_value(&tools).expect("tools serialize");
    let snapshot_path = std::path::Path::new(SNAPSHOT_PATH);
    if std::env::var("UPDATE_SNAPSHOT").ok().as_deref() == Some("1") {
        if let Some(parent) = snapshot_path.parent() {
            std::fs::create_dir_all(parent).expect("create snapshot directory");
        }
        std::fs::write(
            snapshot_path,
            serde_json::to_string_pretty(&value).unwrap() + "\n",
        )
        .expect("write snapshot");
        return;
    }
    let raw = std::fs::read_to_string(snapshot_path).unwrap_or_else(|e| {
        panic!("read snapshot {SNAPSHOT_PATH} ({e}); run with UPDATE_SNAPSHOT=1 to create")
    });
    let expected: serde_json::Value = serde_json::from_str(&raw).expect("snapshot is valid JSON");
    assert_eq!(
        value, expected,
        "tools/list drifted from {SNAPSHOT_PATH}; if intentional, regenerate with UPDATE_SNAPSHOT=1"
    );
}

#[tokio::test]
async fn every_tool_description_documents_idempotency_and_tenancy() {
    let client = spawn_mock(Some(KEY_DEFAULT_TENANT.to_owned())).await;
    let tools = client.list_all_tools().await.expect("tools/list succeeds");
    for tool in tools {
        let description = tool.description.unwrap_or_default();
        assert!(
            description.to_lowercase().contains("idempotent")
                || description.to_lowercase().contains("idempotency"),
            "tool {} must document its idempotency semantics",
            tool.name
        );
        assert!(
            description.to_lowercase().contains("tenant"),
            "tool {} must document tenant scoping",
            tool.name
        );
    }
}
