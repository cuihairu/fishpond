//! fishpond-server: service logic behind the status API (F009) — account
//! management, authorization sessions and their state machines — plus the
//! session coordination layer (F011) on top.
//!
//! This crate is the business seam that HTTP (F025) and MCP both sit on:
//! everything here is tenant-scoped, drives the domain state machines, and
//! persists through [`fishpond_storage`] (which enforces row-level
//! security). No wire formats live here.

pub mod accounts;
pub mod coordination;
pub mod credentials;
pub mod error;
pub mod http;
pub mod sessions;
pub mod takeover;

pub use accounts::{Accounts, StatusSummary};
pub use coordination::SessionCoordinator;
pub use credentials::Credentials;
pub use error::{ServiceError, ServiceResult};
pub use http::{AppState, build_router};
pub use sessions::Sessions;
pub use takeover::{MAX_TTL_MS, TakeoverContext, TakeoverService};
