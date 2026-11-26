//! The REST face (F025): an axum router over the service logic in this
//! crate, with RFC 9457 Problem Details errors, bearer-key auth bound
//! to a tenant, cursor pagination on collections, idempotent POSTs and
//! a served OpenAPI document. Wire types are local to this module —
//! the domain stays dependency-free and the MCP face keeps its own
//! DTOs.
//!
//! The router is transport-only: every handler is a thin call into
//! `Accounts`/`Sessions`/`SessionCoordinator`/storage plus a mapping
//! into [`problem::Problem`] or a JSON DTO.

pub mod auth;
pub mod openapi;
pub mod problem;

use axum::Json;
use axum::Router;
use axum::routing::get;
use serde::Serialize;

use auth::RestKeys;
use problem::Problem;

/// Shared state behind every route.
#[derive(Debug, Clone)]
pub struct AppState {
    /// Bearer-key registry (tenant + scopes per key).
    pub keys: RestKeys,
}

/// The `GET /healthz` body.
#[derive(Debug, Serialize)]
pub struct Health {
    /// Always `ok` when the process answers at all.
    pub status: &'static str,
    /// The serving crate.
    pub service: &'static str,
    /// Crate version.
    pub version: &'static str,
}

/// Builds the REST router for `state`.
pub fn build_router(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(health))
        .route("/openapi.json", get(openapi::serve))
        .with_state(state)
}

/// Liveness: the process is up. Readiness (database reachable) is the
/// monitor's job (F027), not a liveness probe's.
async fn health() -> Json<Health> {
    Json(Health {
        status: "ok",
        service: "fishpond-rest",
        version: env!("CARGO_PKG_VERSION"),
    })
}

/// Convenience alias for handlers: everything failing maps to a
/// [`Problem`].
pub type HandlerResult<T> = Result<T, Problem>;

/// Re-exported so route modules share one extractor.
pub use auth::Caller;
