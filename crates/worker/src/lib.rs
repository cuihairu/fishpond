//! fishpond-worker: Worker daemon — registration, heartbeat and
//! lease/generation coordination (F012).
//!
//! Two thin, typed services sit on the storage CAS surface:
//! [`Registration`] (T01: register, heartbeat, liveness detection) and
//! [`LeaseManager`] (T02: acquire/renew/release; T20: steal and
//! generation fencing). They are stateless — the `Store` owns the pool
//! — so they are cheap to clone and to share. Daemon-side loops
//! (heartbeat intervals, renew scheduling, steal-back policy) are
//! scheduler work (F013), not part of these surfaces.

mod lease;
mod registration;

pub use lease::LeaseManager;
pub use registration::Registration;
