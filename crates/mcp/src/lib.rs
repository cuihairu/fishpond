//! fishpond-mcp: MCP (Model Context Protocol) server face for Fishpond.
//!
//! MCP is the second external protocol face next to REST `/api/v1` (§13):
//! it lets coding agents query accounts/conversations/messages, products,
//! operations and events, and send messages through the Mock provider.
//!
//! Contract (F009M):
//! - Tools are 1:1 wrappers of REST semantics; no business logic lives here.
//! - Authorization reuses the API key + scope model; the stdio transport
//!   reads `FISHPOND_API_KEY`, the HTTP transport reads request headers.
//! - Secrets never appear in logs, errors or tool output (§安全红线).
//! - The only CI target is Mock (§7.3); real providers stay disabled (F032).
//!
//! The service seam ([`services`]) is the integration point for the real
//! server service layer (F025); [`mock`] provides the Mock implementation.

pub mod auth;
pub mod cli;
pub mod mock;
pub mod redact;
pub mod server;
pub mod services;
pub mod tools;

pub use auth::{ApiKeyRecord, ApiKeyStore, AuthContext, ENV_API_KEY, Scope};
pub use mock::MockServices;
pub use server::{http_router, run_stdio, serve_http};
pub use tools::FishpondMcp;
