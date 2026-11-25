//! Transports for the MCP face: stdio (agent-managed child process) and
//! streamable HTTP (endpoint `POST /mcp`).

use rmcp::ServiceExt;
use rmcp::transport::stdio;
use rmcp::transport::streamable_http_server::{
    StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
};

use crate::tools::FishpondMcp;

/// Path the HTTP transport is nested under.
pub const HTTP_ENDPOINT: &str = "/mcp";

/// Serves `handler` over stdio until the peer disconnects.
///
/// stdout carries MCP protocol messages; logs go to stderr only.
pub async fn run_stdio(handler: FishpondMcp) -> Result<(), Box<dyn std::error::Error>> {
    let service = handler.serve(stdio()).await.inspect_err(|e| {
        tracing::error!(target: "fishpond::mcp", error = %e, "stdio transport failed to start");
    })?;
    service.waiting().await?;
    Ok(())
}

/// Builds the axum router serving `handler` at [`HTTP_ENDPOINT`].
pub fn http_router(handler: FishpondMcp) -> axum::Router {
    let service = StreamableHttpService::new(
        move || Ok(handler.clone()),
        LocalSessionManager::default().into(),
        StreamableHttpServerConfig::default(),
    );
    axum::Router::new().nest_service(HTTP_ENDPOINT, service)
}

/// Serves `handler` over streamable HTTP on `bind`.
pub async fn serve_http(
    handler: FishpondMcp,
    bind: std::net::SocketAddr,
) -> Result<(), Box<dyn std::error::Error>> {
    let listener = tokio::net::TcpListener::bind(bind).await?;
    tracing::info!(target: "fishpond::mcp", endpoint = %format!("http://{bind}{HTTP_ENDPOINT}"), "streamable http transport listening");
    axum::serve(listener, http_router(handler)).await?;
    Ok(())
}
