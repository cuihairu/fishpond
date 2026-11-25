//! Result semantics: how provider outcomes classify for retry policy.

use std::fmt;

/// Retry classification of a provider failure.
///
/// Callers (operations/scheduler) decide *whether* to retry; the class is
/// the provider-sdk's contract for *how* a failure may be treated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ErrorClass {
    /// Transient infrastructure failure; safe to retry immediately.
    Transient,
    /// Upstream rate limit; retry after a backoff.
    RateLimited,
    /// Deadline expired mid-call; retry in a new attempt.
    Timeout,
    /// Session/authorization lost; re-authorize before retrying.
    Auth,
    /// The provider does not implement this capability/operation; no retry.
    Unsupported,
    /// Caller-side mistake (bad arguments, unknown resource); no retry.
    Invalid,
    /// Provider-side invariant violation; no automatic retry.
    Internal,
}

impl ErrorClass {
    /// Whether an automatic retry of the same call may succeed.
    pub fn retryable(self) -> bool {
        matches!(
            self,
            Self::Transient | Self::RateLimited | Self::Timeout | Self::Auth
        )
    }

    /// Canonical wire name.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Transient => "transient",
            Self::RateLimited => "rate_limited",
            Self::Timeout => "timeout",
            Self::Auth => "auth",
            Self::Unsupported => "unsupported",
            Self::Invalid => "invalid",
            Self::Internal => "internal",
        }
    }
}

impl fmt::Display for ErrorClass {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A provider failure carrying its retry classification.
///
/// Messages must never contain credentials or session secrets; the redaction
/// contract for logs is pinned by tests in the callers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderError {
    /// Retry classification.
    pub class: ErrorClass,
    /// Stable machine-readable code (e.g. "conversation_not_found").
    pub code: String,
    /// Human-readable, secret-free description.
    pub message: String,
    /// Suggested minimum backoff for [`ErrorClass::RateLimited`], in
    /// milliseconds.
    pub retry_after_ms: Option<u64>,
}

impl ProviderError {
    /// Builds an error; `code` must be snake_case non-empty.
    pub fn new(class: ErrorClass, code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            class,
            code: code.into(),
            message: message.into(),
            retry_after_ms: None,
        }
    }

    /// Attaches a rate-limit backoff hint.
    pub fn with_retry_after(mut self, millis: u64) -> Self {
        self.retry_after_ms = Some(millis);
        self
    }

    /// Whether the caller may retry this failure automatically.
    pub fn retryable(&self) -> bool {
        self.class.retryable()
    }
}

impl fmt::Display for ProviderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} ({}): {}", self.class, self.code, self.message)
    }
}

impl std::error::Error for ProviderError {}

/// Result alias for provider calls.
pub type ProviderResult<T> = Result<T, ProviderError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retry_classification() {
        assert!(ErrorClass::Transient.retryable());
        assert!(ErrorClass::RateLimited.retryable());
        assert!(ErrorClass::Timeout.retryable());
        assert!(ErrorClass::Auth.retryable());
        assert!(!ErrorClass::Unsupported.retryable());
        assert!(!ErrorClass::Invalid.retryable());
        assert!(!ErrorClass::Internal.retryable());
    }

    #[test]
    fn display_is_class_code_message() {
        let err = ProviderError::new(ErrorClass::Invalid, "empty_text", "text must not be empty");
        assert_eq!(
            err.to_string(),
            "invalid (empty_text): text must not be empty"
        );
        assert!(!err.retryable());
    }

    #[test]
    fn rate_limit_carries_backoff_hint() {
        let err = ProviderError::new(ErrorClass::RateLimited, "too_many_requests", "slow down")
            .with_retry_after(1_500);
        assert_eq!(err.retry_after_ms, Some(1_500));
        assert!(err.retryable());
    }
}
