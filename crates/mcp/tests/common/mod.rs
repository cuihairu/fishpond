//! Shared helpers for MCP integration tests: in-memory client/server pair.

#![allow(dead_code)]

use fishpond_mcp::FishpondMcp;
use rmcp::model::CallToolRequestParams;
use rmcp::service::RunningService;
use rmcp::transport::async_rw::AsyncRwTransport;
use rmcp::{RoleClient, RoleServer, ServiceExt};

/// Spawns a `FishpondMcp::mock` server over an in-memory transport and
/// returns a connected MCP client.
///
/// `stdio_key` simulates the `FISHPOND_API_KEY` presented by a stdio peer.
pub async fn spawn_mock(stdio_key: Option<String>) -> RunningService<RoleClient, ()> {
    let handler = FishpondMcp::mock(stdio_key);
    let (server_io, client_io) = tokio::io::duplex(8192);
    // Start the server before the client: the client's `serve_client`
    // performs the initialize handshake, which needs a live server peer.
    let (server_read, server_write) = tokio::io::split(server_io);
    tokio::spawn(async move {
        let server = handler
            .serve(AsyncRwTransport::<RoleServer, _, _>::new_server(
                server_read,
                server_write,
            ))
            .await
            .expect("mcp server initializes");
        server.waiting().await.expect("mcp server loop");
    });
    let (client_read, client_write) = tokio::io::split(client_io);
    rmcp::serve_client((), AsyncRwTransport::new_client(client_read, client_write))
        .await
        .expect("mcp client initializes")
}

/// Calls `tool` with JSON `args` and parses the text output as JSON.
pub async fn call_json(
    client: &RunningService<RoleClient, ()>,
    tool: &str,
    args: serde_json::Value,
) -> serde_json::Value {
    let text = call_text(client, tool, args).await;
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("tool output is not JSON ({e}): {text}"))
}

/// Calls `tool` and returns the raw text content.
pub async fn call_text(
    client: &RunningService<RoleClient, ()>,
    tool: &str,
    args: serde_json::Value,
) -> String {
    let mut params = CallToolRequestParams::new(tool.to_owned());
    params.arguments = args.as_object().cloned();
    let result = client
        .call_tool(params)
        .await
        .unwrap_or_else(|e| panic!("tool {tool} failed: {e}"));
    match result.content.first() {
        Some(rmcp::model::ContentBlock::Text(text)) => text.text.clone(),
        other => panic!("tool {tool}: expected text content, got {other:?}"),
    }
}

/// Calls `tool` expecting a JSON-RPC error; returns the error message.
pub async fn call_error(
    client: &RunningService<RoleClient, ()>,
    tool: &str,
    args: serde_json::Value,
) -> String {
    let mut params = CallToolRequestParams::new(tool.to_owned());
    params.arguments = args.as_object().cloned();
    match client.call_tool(params).await {
        Ok(result) => panic!("tool {tool} unexpectedly succeeded: {result:?}"),
        Err(e) => e.to_string(),
    }
}
