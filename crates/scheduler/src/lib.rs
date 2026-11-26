//! fishpond-scheduler: Capacity- and affinity-aware account scheduling, drain and failover.

pub mod placement;
pub mod scheduler;

pub use placement::{PlacementPolicy, WorkerView};
pub use scheduler::{
    Assignment, DrainReport, MigrationReport, Scheduler, SchedulerError, SchedulerResult,
    TakeoverReport,
};
