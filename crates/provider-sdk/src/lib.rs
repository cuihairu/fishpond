//! fishpond-provider-sdk: the provider seam every engine implements (§3.3).
//!
//! This crate is std-async only: it fixes the capability/manifest/result
//! vocabulary and the [`Provider`] trait, plus a reusable contract suite so
//! provider-mock (F005) and any future real provider are tested against the
//! same black-box invariants.

pub mod capability;
pub mod contract;
pub mod manifest;
pub mod provider;
pub mod result;
pub mod version;

pub use capability::Capability;
pub use contract::{CheckResult, ContractFixture, ContractReport, run_contract_suite};
pub use manifest::{ManifestError, ProviderManifest};
pub use provider::{
    AuthorizeOutcome, CallContext, Provider, SendMessageOutcome, SendMessageRequest,
};
pub use result::{ErrorClass, ProviderError, ProviderResult};
pub use version::{EngineKind, NegotiateError, ProtocolVersion, negotiate};
