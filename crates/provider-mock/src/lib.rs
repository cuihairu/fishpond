//! fishpond-provider-mock: the Mock Platform and Mock Provider — the
//! default and only CI target (§7.3).
//!
//! [`MockPlatform`] simulates the upstream world (accounts, threads,
//! history, catalog, event log) with four injectable fault switches;
//! [`MockProvider`] adapts it to the `fishpond_provider_sdk::Provider`
//! seam and passes that crate's contract suite.

pub mod platform;
pub mod provider;

pub use platform::{MockFault, MockPlatform, SEED_ACCOUNT, SEED_CONVERSATION, SEED_TENANT};
pub use provider::MockProvider;
