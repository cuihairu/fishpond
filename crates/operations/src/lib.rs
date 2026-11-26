//! fishpond-operations: Operation intents, attempts, deadlines, idempotency
//! and result persistence — plus the reconcile loop (F015) that resolves
//! uncertainty on top of that persistence: reaping expired attempts,
//! probing unverified accounts (`UNKNOWN`) and the manual-verification
//! entry, all through the domain state machines with an audit trail.
//!
//! This crate sits on the service seam (§3.3): it may build on
//! `fishpond-domain` and `fishpond-storage` only — never on the
//! delivery/front layers (`server`/`worker`/`router`), which call *into*
//! these services.

pub mod reconcile;

pub use reconcile::{
    AccountProbe, Contested, ProbeOutcome, ReapReport, Reaped, ReconcileError, ReconcileResult,
    ReconcileStep, Reconciler,
};
