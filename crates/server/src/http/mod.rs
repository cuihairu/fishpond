//! The REST face (F025): an axum router over the service logic in this
//! crate, with RFC 9457 Problem Details errors, bearer-key auth bound
//! to a tenant, cursor pagination on collections, idempotent POSTs and
//! a served OpenAPI document. Wire types are local to this module —
//! the domain stays dependency-free and the MCP face keeps its own
//! DTOs.
//!
//! The router is transport-only: every handler is a thin call into
//! `Accounts`/`Sessions`/storage plus a mapping into
//! [`problem::Problem`] or a JSON DTO.

pub mod accounts;
pub mod auth;
pub mod cursor;
pub mod openapi;
pub mod problem;
pub mod sessions;

use std::time::{SystemTime, UNIX_EPOCH};

use axum::routing::{get, post};
use axum::{Extension, Json, Router};
use fishpond_domain::UnixMillis;
use fishpond_storage::Store;
use serde::Serialize;

use auth::RestKeys;
use problem::Problem;

/// Shared state behind every route.
#[derive(Clone)]
pub struct AppState {
    /// Bearer-key registry (tenant + scopes per key).
    pub keys: RestKeys,
    /// Account management service.
    pub accounts: crate::Accounts,
    /// Authorization-session service.
    pub sessions: crate::Sessions,
}

impl AppState {
    /// Builds the state over one connected store.
    pub fn new(store: Store, keys: RestKeys) -> Self {
        AppState {
            keys,
            accounts: crate::Accounts::new(store.clone()),
            sessions: crate::Sessions::new(store),
        }
    }
}

/// The current wall-clock instant (handlers stamp transitions with
/// "now"; deterministic instants live in the services' tests).
pub fn now() -> UnixMillis {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock is after the epoch")
        .as_millis() as i64;
    UnixMillis::from_millis(millis).expect("wall clock is representable")
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
        .route("/accounts", get(accounts::list).post(accounts::register))
        .route("/accounts/{id}", get(accounts::get))
        .route("/accounts/{id}/state", post(accounts::set_state))
        .route("/status", get(accounts::status))
        .route("/sessions", get(sessions::list).post(sessions::start))
        .route("/sessions/{id}", get(sessions::get))
        .route("/sessions/{id}/transition", post(sessions::transition))
        .layer(Extension(state.keys.clone()))
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
