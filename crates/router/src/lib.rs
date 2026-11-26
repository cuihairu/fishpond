//! fishpond-router: Capability router (F016) — the single upstream
//! seam. Routes a capability call through five fail-closed gates
//! (RBAC scope → routability → platform → manifest support + protocol
//! negotiation → circuit breaker), executes it, and settles the
//! tenant+platform circuit from a classified outcome: caller faults
//! pass through uncounted, upstream faults count consecutively.
//!
//! Providers register per platform; in CI the registry holds the mock
//! only (§7.3). Real engines (F032+) plug into the same gates. The
//! versioned fleet (F028) is the operator's registration face: signed
//! packages, coexisting versions, canary rollout and rollback.

pub mod breaker;
pub mod classify;
pub mod error;
pub mod router;
pub mod scope;
pub mod versions;

pub use breaker::{BreakerPolicy, BreakerState, CircuitBreaker};
pub use classify::{FailureDecision, decide};
pub use error::{RouteError, RouteResult};
pub use router::{CapabilityRouter, RoutePlan};
pub use scope::{authorize_scope, required_scope};
pub use versions::{CanaryPolicy, FleetAction, FleetChange, FleetError, FleetView, VersionedFleet};
