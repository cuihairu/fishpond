//! fishpond-monitor: the operator plane's watchtower.
//!
//! - [`HealthRegistry`] keeps the last probe result per platform.
//! - [`CapabilitySwitchboard`] is the kill switch per (platform,
//!   capability).
//! - [`IncidentLog`] records problems idempotently (open once, close
//!   once), scoped by tenant ('' = platform-wide).
//! - [`diagnose`]/[`audit`]/[`probe_platform`] turn provider contract
//!   runs into anomalies, incidents and health status.
//! - [`RolloutAuditor`] turns versioned-fleet mutations (promote,
//!   rollback, canary) into operator incidents.
//!
//! The monitor reads and writes deployment-wide tables that sit outside
//! tenant RLS (see `fishpond_storage::monitor`); incidents only carry a
//! tenant reference for listing — it is not a tenancy boundary.

pub mod diagnostics;
pub mod error;
pub mod health;
pub mod incident;
pub mod rollout;
pub mod switchboard;

pub use diagnostics::{ContractAnomaly, ProbeRun, audit, diagnose, probe_platform};
pub use error::{MonitorError, MonitorResult};
pub use health::{HealthRegistry, HealthStatus};
pub use incident::{IncidentDraft, IncidentLog, Severity};
pub use rollout::RolloutAuditor;
pub use switchboard::CapabilitySwitchboard;
