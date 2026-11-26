//! Router errors: what a routed call can fail with, before and after
//! the upstream is touched.
//!
//! Displays are secret-free by construction: they carry capability
//! names, scopes, platform ids and provider error text (whose own
//! contract forbids secrets), never keys or credentials.

use std::fmt;

use fishpond_provider_sdk::{Capability, ProviderError};

/// A capability-routing failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RouteError {
    /// The resolved key lacks the scope this capability requires
    /// (precedence gate 1: checked before anything upstream).
    ScopeDenied {
        /// The missing scope (never the key itself).
        scope: &'static str,
    },
    /// The capability maps to no frozen scope — not routable for any
    /// key, regardless of privileges (gate 2).
    NotRoutable {
        /// The capability that cannot be routed.
        capability: Capability,
    },
    /// No provider is registered for the requested platform (gate 3).
    NoProviderForPlatform {
        /// The platform that has no provider.
        platform: String,
    },
    /// A provider exists for the platform but its manifest does not
    /// declare this capability (gate 4).
    CapabilityNotSupported {
        /// The provider id that lacks the capability.
        provider: String,
        /// The missing capability.
        capability: Capability,
    },
    /// The circuit for this tenant+platform is open: upstream is
    /// presumed down and the call was short-circuited without being
    /// sent. `retry_after_ms` is the earliest instant a probe is
    /// allowed, relative to the caller's clock.
    CircuitOpen {
        /// The breaker key (`tenant/platform`).
        key: String,
        /// Suggested wait in milliseconds.
        retry_after_ms: i64,
    },
    /// The upstream call itself failed; the provider's own retry
    /// classification rides along.
    Upstream(ProviderError),
}

impl fmt::Display for RouteError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ScopeDenied { scope } => write!(f, "scope denied: missing {scope}"),
            Self::NotRoutable { capability } => {
                write!(f, "not routable: no scope gates {capability}")
            }
            Self::NoProviderForPlatform { platform } => {
                write!(f, "no provider for platform {platform}")
            }
            Self::CapabilityNotSupported {
                provider,
                capability,
            } => write!(f, "provider {provider} does not support {capability}"),
            Self::CircuitOpen {
                key,
                retry_after_ms,
            } => write!(f, "circuit open for {key}; retry after {retry_after_ms}ms"),
            Self::Upstream(e) => write!(f, "upstream: {e}"),
        }
    }
}

impl std::error::Error for RouteError {}

impl From<ProviderError> for RouteError {
    fn from(e: ProviderError) -> Self {
        Self::Upstream(e)
    }
}

/// Result alias for router calls.
pub type RouteResult<T> = Result<T, RouteError>;

#[cfg(test)]
mod tests {
    use super::*;
    use fishpond_provider_sdk::ErrorClass;

    #[test]
    fn display_names_the_gate_without_secrets() {
        assert_eq!(
            RouteError::ScopeDenied {
                scope: "conversations:write",
            }
            .to_string(),
            "scope denied: missing conversations:write"
        );
        assert_eq!(
            RouteError::NotRoutable {
                capability: Capability::ManageOrders,
            }
            .to_string(),
            "not routable: no scope gates orders:manage"
        );
        assert_eq!(
            RouteError::NoProviderForPlatform {
                platform: "wx".to_owned(),
            }
            .to_string(),
            "no provider for platform wx"
        );
        assert_eq!(
            RouteError::CapabilityNotSupported {
                provider: "mock".to_owned(),
                capability: Capability::StreamEvents,
            }
            .to_string(),
            "provider mock does not support events:stream"
        );
        assert_eq!(
            RouteError::CircuitOpen {
                key: "t/x".to_owned(),
                retry_after_ms: 40,
            }
            .to_string(),
            "circuit open for t/x; retry after 40ms"
        );
        let upstream = RouteError::Upstream(ProviderError::new(
            ErrorClass::Transient,
            "write_rejected",
            "upstream said no",
        ));
        assert_eq!(
            upstream.to_string(),
            "upstream: transient (write_rejected): upstream said no"
        );
    }
}
