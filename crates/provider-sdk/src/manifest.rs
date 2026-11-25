//! Provider manifests: what a provider is, speaks and can do.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use crate::capability::Capability;
use crate::version::{EngineKind, ProtocolVersion};

/// Static description of a provider implementation.
///
/// Manifests drive capability routing (F016) and digest/rollout checks
/// (F028); they are validated once at load time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderManifest {
    /// Stable provider identifier (e.g. "mock", "official-api").
    pub provider_id: String,
    /// Human-readable name.
    pub display_name: String,
    /// Provider implementation version (semver-ish, at least `x.y`).
    pub provider_version: String,
    /// Engine the provider drives.
    pub engine: EngineKind,
    /// Protocol versions the provider can speak, any order.
    pub protocol_versions: Vec<ProtocolVersion>,
    /// Capabilities the provider implements.
    pub capabilities: Vec<Capability>,
}

/// Why a manifest was rejected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ManifestError {
    /// A required string field was empty.
    EmptyField(&'static str),
    /// The version string is not `x.y` or `x.y.z`.
    BadVersion(String),
    /// No protocol versions were declared.
    NoProtocolVersions,
    /// A capability was declared twice.
    DuplicateCapability(Capability),
    /// No capabilities were declared.
    NoCapabilities,
}

impl std::fmt::Display for ManifestError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EmptyField(name) => write!(f, "manifest field {name} must not be empty"),
            Self::BadVersion(raw) => write!(f, "bad provider_version {raw:?}"),
            Self::NoProtocolVersions => f.write_str("manifest declares no protocol versions"),
            Self::DuplicateCapability(cap) => {
                write!(f, "capability {cap} declared twice")
            }
            Self::NoCapabilities => f.write_str("manifest declares no capabilities"),
        }
    }
}

impl std::error::Error for ManifestError {}

impl ProviderManifest {
    /// Validates the manifest invariants.
    pub fn validate(&self) -> Result<(), ManifestError> {
        if self.provider_id.trim().is_empty() {
            return Err(ManifestError::EmptyField("provider_id"));
        }
        if self.display_name.trim().is_empty() {
            return Err(ManifestError::EmptyField("display_name"));
        }
        if !version_looks_valid(&self.provider_version) {
            return Err(ManifestError::BadVersion(self.provider_version.clone()));
        }
        if self.protocol_versions.is_empty() {
            return Err(ManifestError::NoProtocolVersions);
        }
        if self.capabilities.is_empty() {
            return Err(ManifestError::NoCapabilities);
        }
        let mut seen: Vec<Capability> = Vec::new();
        for cap in &self.capabilities {
            if seen.contains(cap) {
                return Err(ManifestError::DuplicateCapability(*cap));
            }
            seen.push(*cap);
        }
        Ok(())
    }

    /// Whether the provider declares `capability`.
    pub fn supports(&self, capability: Capability) -> bool {
        self.capabilities.contains(&capability)
    }

    /// The highest protocol version the provider speaks.
    pub fn highest_protocol(&self) -> ProtocolVersion {
        self.protocol_versions
            .iter()
            .copied()
            .max()
            .expect("validated manifests declare at least one version")
    }

    /// A stable content digest over the manifest's semantic fields.
    ///
    /// Used by F028 rollout checks to detect silent manifest drift; it is a
    /// plain std hash, not a cryptographic digest.
    pub fn content_digest(&self) -> u64 {
        let mut hasher = DefaultHasher::new();
        self.provider_id.hash(&mut hasher);
        self.provider_version.hash(&mut hasher);
        self.engine.as_str().hash(&mut hasher);
        let mut versions = self.protocol_versions.clone();
        versions.sort_unstable();
        for v in versions {
            v.major.hash(&mut hasher);
            v.minor.hash(&mut hasher);
        }
        let mut caps = self.capabilities.clone();
        caps.sort_unstable();
        for cap in caps {
            cap.as_str().hash(&mut hasher);
        }
        hasher.finish()
    }
}

fn version_looks_valid(raw: &str) -> bool {
    let parts: Vec<&str> = raw.split('.').collect();
    // A bare major is not enough to pin compatibility.
    if !matches!(parts.len(), 2..=3) {
        return false;
    }
    parts
        .iter()
        .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(crate) fn sample_manifest() -> ProviderManifest {
        ProviderManifest {
            provider_id: "mock".to_owned(),
            display_name: "Mock Provider".to_owned(),
            provider_version: "0.1.0".to_owned(),
            engine: EngineKind::Api,
            protocol_versions: vec![ProtocolVersion::V1_0, ProtocolVersion::V1_1],
            capabilities: vec![
                Capability::AccountState,
                Capability::Authorize,
                Capability::ReadConversations,
                Capability::SendMessage,
                Capability::ReadCatalog,
            ],
        }
    }

    #[test]
    fn sample_manifest_is_valid() {
        assert_eq!(sample_manifest().validate(), Ok(()));
    }

    #[test]
    fn rejects_empty_fields_and_bad_versions() {
        let mut m = sample_manifest();
        m.provider_id = "  ".to_owned();
        assert_eq!(m.validate(), Err(ManifestError::EmptyField("provider_id")));

        let mut m = sample_manifest();
        m.provider_version = "1".to_owned();
        assert_eq!(m.validate(), Err(ManifestError::BadVersion("1".to_owned())));

        let mut m = sample_manifest();
        m.provider_version = "1.x".to_owned();
        assert!(matches!(m.validate(), Err(ManifestError::BadVersion(_))));
    }

    #[test]
    fn rejects_missing_versions_capabilities_and_duplicates() {
        let mut m = sample_manifest();
        m.protocol_versions.clear();
        assert_eq!(m.validate(), Err(ManifestError::NoProtocolVersions));

        let mut m = sample_manifest();
        m.capabilities.clear();
        assert_eq!(m.validate(), Err(ManifestError::NoCapabilities));

        let mut m = sample_manifest();
        m.capabilities.push(Capability::SendMessage);
        assert_eq!(
            m.validate(),
            Err(ManifestError::DuplicateCapability(Capability::SendMessage))
        );
    }

    #[test]
    fn support_check_and_highest_protocol() {
        let m = sample_manifest();
        assert!(m.supports(Capability::SendMessage));
        assert!(!m.supports(Capability::ManageOrders));
        assert_eq!(m.highest_protocol(), ProtocolVersion::V1_1);
    }

    #[test]
    fn content_digest_is_order_insensitive_and_drift_sensitive() {
        let base = sample_manifest();
        let mut shuffled = sample_manifest();
        shuffled.protocol_versions.reverse();
        shuffled.capabilities.reverse();
        assert_eq!(base.content_digest(), shuffled.content_digest());

        let mut drifted = sample_manifest();
        drifted.provider_version = "0.2.0".to_owned();
        assert_ne!(base.content_digest(), drifted.content_digest());

        let mut dropped = sample_manifest();
        dropped.capabilities.pop();
        assert_ne!(base.content_digest(), dropped.content_digest());
    }
}
