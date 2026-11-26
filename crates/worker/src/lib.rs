//! fishpond-worker: Worker daemon — registration, heartbeat and
//! lease/generation coordination (F012).
//!
//! Two thin, typed services sit on the storage CAS surface:
//! [`Registration`] (T01: register, heartbeat, liveness detection) and
//! [`LeaseManager`] (T02: acquire/renew/release; T20: steal and
//! generation fencing). They are stateless — the `Store` owns the pool
//! — so they are cheap to clone and to share. The daemon-side loops
//! live in [`KeepAlive`] (F013: heartbeat + lease renewal on an
//! interval, with the one-shot steal-back policy). [`ProfileManager`]
//! (F022: T11 exclusive lock, T12 backup/restore/destroy) adds the
//! on-disk lifecycle for browser profiles. [`EngineSelector`] (F024:
//! T09 dual-engine routing with per-capability downgrade, T25 browser
//! backpressure) picks the lane per call — the gated API engine
//! first, the page worker as the degraded path.

mod engine;
mod keepalive;
mod lease;
mod profile;
mod registration;

pub use engine::{
    BackpressureGate, BrowserFault, BrowserLane, EngineError, EngineRequest, EngineSelector,
};
pub use keepalive::{KeepAlive, KeepAliveConfig, RenewOutcome, TickReport};
pub use lease::LeaseManager;
pub use profile::{
    BackupReport, MANIFEST_NAME, MigrateReport, MigrationTarget, ProfileError, ProfileManager,
};
pub use registration::Registration;
