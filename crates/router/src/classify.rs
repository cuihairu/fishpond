//! Error classification (F016): how a provider failure is treated by
//! the router — and in particular whether it counts against the
//! circuit breaker.
//!
//! The provider-sdk already classifies every failure into an
//! [`ErrorClass`]; this module turns that classification into a
//! *routing decision*:
//!
//! * **Caller faults** (`Invalid`, `Unsupported`) are the caller's or
//!   the config's fault — the upstream answered fine. They pass
//!   through untouched and never count toward the breaker: a run of
//!   malformed requests must not take a healthy provider offline.
//! * **Upstream faults** (everything else — transient losses, rate
//!   limits, timeouts, auth loss, provider bugs) mean the upstream
//!   itself may be unhealthy. They return to the caller as-is *and*
//!   count a consecutive failure on the tenant+platform circuit.
//!
//! [`ErrorClass::Auth`] counts: a lost session is real upstream state
//! even though the *remedy* (re-authorize) is caller-side.

use fishpond_provider_sdk::{ErrorClass, ProviderError};

/// The routing decision for one provider failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureDecision {
    /// The upstream is healthy; the request was bad. Pass the error
    /// through and leave the breaker alone.
    CallerFault,
    /// The upstream itself failed. Pass the error through and count a
    /// consecutive failure on the circuit.
    UpstreamFault,
}

/// Classifies one provider failure.
pub fn decide(error: &ProviderError) -> FailureDecision {
    match error.class {
        ErrorClass::Invalid | ErrorClass::Unsupported => FailureDecision::CallerFault,
        ErrorClass::Transient
        | ErrorClass::RateLimited
        | ErrorClass::Timeout
        | ErrorClass::Auth
        | ErrorClass::Internal => FailureDecision::UpstreamFault,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn caller_faults_pass_through_without_counting() {
        for class in [ErrorClass::Invalid, ErrorClass::Unsupported] {
            let decision = decide(&ProviderError::new(class, "x", "y"));
            assert_eq!(decision, FailureDecision::CallerFault, "{class}");
        }
    }

    #[test]
    fn upstream_faults_count_toward_the_breaker() {
        for class in [
            ErrorClass::Transient,
            ErrorClass::RateLimited,
            ErrorClass::Timeout,
            ErrorClass::Auth,
            ErrorClass::Internal,
        ] {
            let decision = decide(&ProviderError::new(class, "x", "y"));
            assert_eq!(decision, FailureDecision::UpstreamFault, "{class}");
        }
    }
}
