//! Account and operation state machines.
//!
//! `UNKNOWN` is an *observational* state: the upstream platform could not be
//! reached or answered inconsistently, so the account is neither confirmed
//! online nor offline. It is a real state that consumers must surface (never
//! silently map to online/offline), and reconcile jobs move it back to a
//! confirmed state.

use crate::error::{DomainError, DomainResult};

/// Lifecycle state of one upstream platform account.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AccountState {
    /// Confirmed online: session works and the upstream accepts calls.
    Online,
    /// Confirmed offline: no usable session.
    Offline,
    /// Observed state could not be verified (neither confirmed online nor
    /// offline) — see module docs.
    Unknown,
    /// Administratively disabled: no transitions out except explicit
    /// re-enable via [`AccountState::DISABLED_TRANSITIONS`] (none today; the
    /// only legal exit is a fresh account lifecycle).
    Disabled,
}

impl AccountState {
    /// Allowed transitions from this state (the state machine table).
    pub const TRANSITIONS: &'static [(&'static AccountState, &'static [AccountState])] = &[
        (
            &AccountState::Online,
            &[
                AccountState::Offline,
                AccountState::Unknown,
                AccountState::Disabled,
            ],
        ),
        (
            &AccountState::Offline,
            &[
                AccountState::Online,
                AccountState::Unknown,
                AccountState::Disabled,
            ],
        ),
        (
            &AccountState::Unknown,
            &[
                AccountState::Online,
                AccountState::Offline,
                AccountState::Disabled,
            ],
        ),
        (&AccountState::Disabled, &[]),
    ];

    /// Canonical wire name of the state (REST/MCP expose this string).
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Online => "ONLINE",
            Self::Offline => "OFFLINE",
            Self::Unknown => "UNKNOWN",
            Self::Disabled => "DISABLED",
        }
    }

    /// Parses the canonical wire name (case-sensitive).
    pub fn parse(raw: &str) -> DomainResult<Self> {
        match raw {
            "ONLINE" => Ok(Self::Online),
            "OFFLINE" => Ok(Self::Offline),
            "UNKNOWN" => Ok(Self::Unknown),
            "DISABLED" => Ok(Self::Disabled),
            other => Err(DomainError::InvalidStateName {
                machine: "account",
                raw: other.to_owned(),
            }),
        }
    }

    /// Applies one transition, enforcing the table.
    ///
    /// Note: staying in the same state is NOT a transition — callers should
    /// not call this when nothing changed; it is rejected as illegal so that
    /// silent no-ops cannot mask a broken observation loop.
    pub fn transition_to(self, next: AccountState) -> DomainResult<AccountState> {
        let allowed = Self::TRANSITIONS
            .iter()
            .find(|(from, _)| **from == self)
            .map(|(_, to)| *to)
            .unwrap_or(&[]);
        if allowed.contains(&next) {
            Ok(next)
        } else {
            Err(DomainError::InvalidTransition {
                machine: "account",
                from: self.as_str(),
                to: next.as_str(),
            })
        }
    }
}

impl std::fmt::Display for AccountState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Execution state of one operation intent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum OperationState {
    /// Created, not yet claimed by a worker.
    Pending,
    /// Claimed and executing inside an attempt.
    Running,
    /// Terminal success.
    Succeeded,
    /// Terminal failure (all attempts exhausted or explicitly failed).
    Failed,
    /// Terminal cancel requested by an operator.
    Cancelled,
}

impl OperationState {
    /// Allowed transitions from this state.
    pub const TRANSITIONS: &'static [(&'static OperationState, &'static [OperationState])] = &[
        (
            &OperationState::Pending,
            &[OperationState::Running, OperationState::Cancelled],
        ),
        (
            &OperationState::Running,
            &[
                OperationState::Succeeded,
                OperationState::Failed,
                OperationState::Cancelled,
            ],
        ),
        (&OperationState::Succeeded, &[]),
        (&OperationState::Failed, &[]),
        (&OperationState::Cancelled, &[]),
    ];

    /// Canonical wire name of the state.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Pending => "PENDING",
            Self::Running => "RUNNING",
            Self::Succeeded => "SUCCEEDED",
            Self::Failed => "FAILED",
            Self::Cancelled => "CANCELLED",
        }
    }

    /// Whether the state is terminal (no further transitions).
    pub fn is_terminal(self) -> bool {
        self.next_states().is_empty()
    }

    fn next_states(self) -> &'static [OperationState] {
        Self::TRANSITIONS
            .iter()
            .find(|(from, _)| **from == self)
            .map(|(_, to)| *to)
            .unwrap_or(&[])
    }

    /// Applies one transition, enforcing the table.
    pub fn transition_to(self, next: OperationState) -> DomainResult<OperationState> {
        if self.next_states().contains(&next) {
            Ok(next)
        } else {
            Err(DomainError::InvalidTransition {
                machine: "operation",
                from: self.as_str(),
                to: next.as_str(),
            })
        }
    }
}

impl std::fmt::Display for OperationState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn account_online_transitions_are_legal() {
        let online = AccountState::Online;
        assert_eq!(
            online.transition_to(AccountState::Offline),
            Ok(AccountState::Offline)
        );
        assert_eq!(
            online.transition_to(AccountState::Unknown),
            Ok(AccountState::Unknown)
        );
        assert_eq!(
            online.transition_to(AccountState::Disabled),
            Ok(AccountState::Disabled)
        );
    }

    #[test]
    fn account_disabled_is_terminal() {
        for next in [
            AccountState::Online,
            AccountState::Offline,
            AccountState::Unknown,
            AccountState::Disabled,
        ] {
            assert_eq!(
                AccountState::Disabled.transition_to(next),
                Err(DomainError::InvalidTransition {
                    machine: "account",
                    from: "DISABLED",
                    to: next.as_str(),
                }),
                "disabled must have no exits"
            );
        }
    }

    #[test]
    fn account_same_state_is_illegal_no_op() {
        let err = AccountState::Online
            .transition_to(AccountState::Online)
            .expect_err("self transition must be rejected");
        assert_eq!(
            err.to_string(),
            "illegal account state transition: ONLINE -> ONLINE"
        );
    }

    #[test]
    fn account_unknown_recovers_to_confirmed_states() {
        assert_eq!(
            AccountState::Unknown.transition_to(AccountState::Online),
            Ok(AccountState::Online)
        );
        assert_eq!(
            AccountState::Unknown.transition_to(AccountState::Offline),
            Ok(AccountState::Offline)
        );
    }

    #[test]
    fn account_state_wire_names_roundtrip() {
        for state in [
            AccountState::Online,
            AccountState::Offline,
            AccountState::Unknown,
            AccountState::Disabled,
        ] {
            assert_eq!(AccountState::parse(state.as_str()), Ok(state));
            assert_eq!(state.to_string(), state.as_str());
        }
        assert!(AccountState::parse("online").is_err());
        assert!(AccountState::parse("ON").is_err());
    }

    #[test]
    fn operation_happy_path_is_legal() {
        let mut state = OperationState::Pending;
        state = state.transition_to(OperationState::Running).unwrap();
        state = state.transition_to(OperationState::Succeeded).unwrap();
        assert!(state.is_terminal());
    }

    #[test]
    fn operation_pending_cannot_succeed_without_running() {
        assert!(
            OperationState::Pending
                .transition_to(OperationState::Succeeded)
                .is_err()
        );
        assert!(
            OperationState::Pending
                .transition_to(OperationState::Failed)
                .is_err()
        );
    }

    #[test]
    fn operation_running_can_fail_or_cancel_and_cancel_from_pending() {
        assert!(
            OperationState::Running
                .transition_to(OperationState::Failed)
                .is_ok()
        );
        assert!(
            OperationState::Running
                .transition_to(OperationState::Cancelled)
                .is_ok()
        );
        assert!(
            OperationState::Pending
                .transition_to(OperationState::Cancelled)
                .is_ok()
        );
    }

    #[test]
    fn operation_terminal_states_have_no_exits() {
        for terminal in [
            OperationState::Succeeded,
            OperationState::Failed,
            OperationState::Cancelled,
        ] {
            assert!(terminal.is_terminal());
            for next in [
                OperationState::Pending,
                OperationState::Running,
                OperationState::Succeeded,
                OperationState::Failed,
                OperationState::Cancelled,
            ] {
                assert_eq!(
                    terminal.transition_to(next),
                    Err(DomainError::InvalidTransition {
                        machine: "operation",
                        from: terminal.as_str(),
                        to: next.as_str(),
                    })
                );
            }
        }
    }
}
