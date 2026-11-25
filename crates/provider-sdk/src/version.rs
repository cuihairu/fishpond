//! Provider protocol versions and negotiation.

use std::fmt;

/// Which engine a provider implementation drives.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EngineKind {
    /// Official platform HTTP API (F032, authorization-gated).
    Api,
    /// Browser automation (F021+, authorization-gated).
    Browser,
}

impl EngineKind {
    /// Canonical wire name.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Api => "api",
            Self::Browser => "browser",
        }
    }
}

impl fmt::Display for EngineKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A `major.minor` protocol version (compared numerically, minor-first
/// compatibility: same major, client's minor may be lower).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ProtocolVersion {
    /// Breaking-change component.
    pub major: u16,
    /// Backward-compatible component.
    pub minor: u16,
}

impl ProtocolVersion {
    /// Builds a version; both components must be non-zero only in major
    /// (minor 0 is legal, major 0 is not a released protocol).
    pub const V1_0: ProtocolVersion = ProtocolVersion { major: 1, minor: 0 };
    /// Version 1.1.
    pub const V1_1: ProtocolVersion = ProtocolVersion { major: 1, minor: 1 };
    /// Version 2.0.
    pub const V2_0: ProtocolVersion = ProtocolVersion { major: 2, minor: 0 };

    /// Parses `"<major>.<minor>"`.
    pub fn parse(raw: &str) -> Option<Self> {
        let (major, minor) = raw.split_once('.')?;
        let major: u16 = major.parse().ok()?;
        let minor: u16 = minor.parse().ok()?;
        if major == 0 {
            return None;
        }
        Some(Self { major, minor })
    }

    /// The highest minor this major is wire-compatible with for a peer
    /// speaking the same major (all minors of one major interoperate:
    /// the lower minor governs the feature set).
    pub fn compatible_floor(self, other: ProtocolVersion) -> Option<ProtocolVersion> {
        if self.major == other.major {
            Some(std::cmp::min(self, other))
        } else {
            None
        }
    }
}

impl fmt::Display for ProtocolVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}", self.major, self.minor)
    }
}

/// Why negotiation failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NegotiateError {
    /// No protocol version is shared by caller and provider.
    NoOverlap {
        /// Versions the caller offered, sorted descending.
        offered: Vec<ProtocolVersion>,
        /// Versions the provider supports, sorted descending.
        supported: Vec<ProtocolVersion>,
    },
    /// The caller offered no versions at all.
    EmptyOffer,
}

impl fmt::Display for NegotiateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoOverlap { offered, supported } => write!(
                f,
                "no shared protocol version: offered [{}], provider supports [{}]",
                join_versions(offered),
                join_versions(supported)
            ),
            Self::EmptyOffer => f.write_str("caller offered no protocol versions"),
        }
    }
}

impl std::error::Error for NegotiateError {}

fn join_versions(versions: &[ProtocolVersion]) -> String {
    versions
        .iter()
        .map(|v| v.to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

/// Picks the highest version shared by `offer` and the manifest, honouring
/// major-version compatibility (the shared floor wins within a major).
pub fn negotiate(
    offer: &[ProtocolVersion],
    supported: &[ProtocolVersion],
) -> Result<ProtocolVersion, NegotiateError> {
    if offer.is_empty() {
        return Err(NegotiateError::EmptyOffer);
    }
    let mut offered: Vec<ProtocolVersion> = offer.to_vec();
    offered.sort_unstable_by(|a, b| b.cmp(a));
    let mut provider_side: Vec<ProtocolVersion> = supported.to_vec();
    provider_side.sort_unstable_by(|a, b| b.cmp(a));

    for candidate in offered.iter().copied() {
        if let Some(matched) = provider_side
            .iter()
            .copied()
            .find(|s| s.compatible_floor(candidate).is_some())
        {
            // Highest offered major that the provider speaks: the floor is
            // the negotiated feature set.
            return Ok(candidate.min(matched));
        }
    }
    Err(NegotiateError::NoOverlap {
        offered,
        supported: provider_side,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_and_display_roundtrip() {
        assert_eq!(ProtocolVersion::parse("1.0"), Some(ProtocolVersion::V1_0));
        assert_eq!(ProtocolVersion::parse("1.1"), Some(ProtocolVersion::V1_1));
        assert_eq!(ProtocolVersion::V1_1.to_string(), "1.1");
        assert_eq!(ProtocolVersion::parse("2").map(|_| ()), None);
        assert_eq!(ProtocolVersion::parse("1.x").map(|_| ()), None);
        assert_eq!(ProtocolVersion::parse("0.1").map(|_| ()), None);
    }

    #[test]
    fn negotiation_picks_highest_shared_major_with_floor_minor() {
        // provider supports 1.1 and 2.0; caller offers 1.0 and 1.1
        let agreed = negotiate(
            &[ProtocolVersion::V1_0, ProtocolVersion::V1_1],
            &[ProtocolVersion::V1_1, ProtocolVersion::V2_0],
        )
        .unwrap();
        assert_eq!(agreed, ProtocolVersion::V1_1);
        // caller only speaks 1.0 → floor is 1.0 even though provider has 1.1
        let agreed = negotiate(&[ProtocolVersion::V1_0], &[ProtocolVersion::V1_1]).unwrap();
        assert_eq!(agreed, ProtocolVersion::V1_0);
    }

    #[test]
    fn negotiation_reports_no_overlap() {
        let err = negotiate(&[ProtocolVersion::V1_0], &[ProtocolVersion::V2_0]).unwrap_err();
        assert!(err.to_string().contains("no shared protocol version"));
        match err {
            NegotiateError::NoOverlap { offered, supported } => {
                assert_eq!(offered, vec![ProtocolVersion::V1_0]);
                assert_eq!(supported, vec![ProtocolVersion::V2_0]);
            }
            other => panic!("expected NoOverlap, got {other:?}"),
        }
    }

    #[test]
    fn negotiation_rejects_empty_offer() {
        assert_eq!(
            negotiate(&[], &[ProtocolVersion::V1_0]),
            Err(NegotiateError::EmptyOffer)
        );
    }

    #[test]
    fn negotiation_prefers_higher_major_when_both_share_it() {
        // both speak 2.x → 2.0 wins over the shared 1.1
        let agreed = negotiate(
            &[ProtocolVersion::V1_1, ProtocolVersion::V2_0],
            &[ProtocolVersion::V1_1, ProtocolVersion::V2_0],
        )
        .unwrap();
        assert_eq!(agreed, ProtocolVersion::V2_0);
    }
}
