//! fishpond-server: service logic behind the status API (F009) — account
//! management, authorization sessions and their state machines — plus the
//! session coordination layer (F011) on top.
//!
//! This crate is the business seam that HTTP (F025) and MCP both sit on:
//! everything here is tenant-scoped, drives the domain state machines, and
//! persists through [`fishpond_storage`] (which enforces row-level
//! security). No wire formats live here.

pub mod accounts;
pub mod error;
pub mod sessions;

pub use accounts::{Accounts, StatusSummary};
pub use error::{ServiceError, ServiceResult};
pub use sessions::Sessions;
