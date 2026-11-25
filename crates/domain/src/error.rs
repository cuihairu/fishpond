//! Error types for the domain layer (std-only, no external crates).

use std::fmt;

/// Result alias for domain operations.
pub type DomainResult<T> = Result<T, DomainError>;

/// Errors produced by domain rules: id validation and state transitions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DomainError {
    /// A resource id failed validation.
    InvalidId {
        /// Which kind of id ("account", "tenant", ...).
        what: &'static str,
        /// The raw input that was rejected.
        raw: String,
        /// Why it was rejected.
        reason: IdInvalidReason,
    },
    /// A state transition is not allowed by the state machine.
    InvalidTransition {
        /// Machine the transition belongs to ("account", "operation").
        machine: &'static str,
        /// Current state.
        from: &'static str,
        /// Requested state.
        to: &'static str,
    },
    /// A text field that must carry content was empty or whitespace-only.
    EmptyText {
        /// Which field was empty.
        field: &'static str,
    },
    /// A state name did not match any state of the machine.
    InvalidStateName {
        /// Which machine ("account", "operation").
        machine: &'static str,
        /// The raw input that was rejected.
        raw: String,
    },
    /// A numeric value was outside its allowed range.
    OutOfRange {
        /// Which field was out of range.
        field: &'static str,
        /// Human-readable constraint, e.g. "must be >= 0".
        constraint: &'static str,
    },
}

/// Why an id string was rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdInvalidReason {
    /// Empty string.
    Empty,
    /// Longer than [`crate::id::MAX_ID_LEN`].
    TooLong,
    /// Characters outside `[A-Za-z0-9_-]`.
    BadCharset,
}

impl fmt::Display for IdInvalidReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => f.write_str("empty"),
            Self::TooLong => f.write_str("too long"),
            Self::BadCharset => f.write_str("invalid characters (allowed: A-Z a-z 0-9 _ -)"),
        }
    }
}

impl fmt::Display for DomainError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidId { what, raw, reason } => {
                write!(f, "invalid {what} id {raw:?}: {reason}")
            }
            Self::InvalidTransition { machine, from, to } => {
                write!(f, "illegal {machine} state transition: {from} -> {to}")
            }
            Self::EmptyText { field } => write!(f, "{field} must not be empty"),
            Self::InvalidStateName { machine, raw } => {
                write!(f, "unknown {machine} state: {raw:?}")
            }
            Self::OutOfRange { field, constraint } => {
                write!(f, "{field} out of range: {constraint}")
            }
        }
    }
}

impl std::error::Error for DomainError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_mentions_machine_states_and_direction() {
        let err = DomainError::InvalidTransition {
            machine: "account",
            from: "disabled",
            to: "online",
        };
        assert_eq!(
            err.to_string(),
            "illegal account state transition: disabled -> online"
        );
    }

    #[test]
    fn display_reports_id_reason() {
        let err = DomainError::InvalidId {
            what: "account",
            raw: "bad id!".to_owned(),
            reason: IdInvalidReason::BadCharset,
        };
        let text = err.to_string();
        assert!(text.contains("account"));
        assert!(text.contains("bad id!"));
        assert!(text.contains("invalid characters"));
    }

    #[test]
    fn display_reports_text_and_range_errors() {
        assert_eq!(
            DomainError::EmptyText { field: "text" }.to_string(),
            "text must not be empty"
        );
        assert_eq!(
            DomainError::OutOfRange {
                field: "price_cents",
                constraint: "must be >= 0",
            }
            .to_string(),
            "price_cents out of range: must be >= 0"
        );
    }
}
