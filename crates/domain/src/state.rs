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

/// Lifecycle of one authorization session (login of an account on its
/// platform).
///
/// A session starts when the platform login flow begins (e.g. a QR code is
/// issued) and becomes [`AuthSessionState::Authorized`] once the platform
/// confirms credentials. `AWAITING_CONFIRMATION` is the platform-side step
/// where the user has acted (scanned the code) but the confirmation
/// callback has not arrived yet. Terminal states keep their record for
/// audit; a re-login always starts a *new* session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AuthSessionState {
    /// Login flow started, waiting on the platform/user.
    Pending,
    /// The user acted (e.g. scanned the code); the confirmation callback
    /// has not arrived yet.
    AwaitingConfirmation,
    /// Platform confirmed the login; the session holds usable credentials.
    Authorized,
    /// Timed out (login not completed in time, or session TTL elapsed).
    Expired,
    /// Explicitly revoked by an operator or the platform.
    Revoked,
    /// Login flow failed (platform rejected, callback error, ...).
    Failed,
    /// An operator took the session over manually: automation must stop
    /// touching the account until the session is resumed (F011).
    TakenOver,
}

impl AuthSessionState {
    /// Allowed transitions from this state.
    pub const TRANSITIONS: &'static [(&'static AuthSessionState, &'static [AuthSessionState])] = &[
        (
            &AuthSessionState::Pending,
            &[
                AuthSessionState::AwaitingConfirmation,
                AuthSessionState::Authorized,
                AuthSessionState::Expired,
                AuthSessionState::Revoked,
                AuthSessionState::Failed,
            ],
        ),
        (
            &AuthSessionState::AwaitingConfirmation,
            &[
                AuthSessionState::Authorized,
                AuthSessionState::Expired,
                AuthSessionState::Revoked,
                AuthSessionState::Failed,
            ],
        ),
        (
            &AuthSessionState::Authorized,
            &[
                AuthSessionState::Expired,
                AuthSessionState::Revoked,
                AuthSessionState::TakenOver,
            ],
        ),
        (
            &AuthSessionState::TakenOver,
            &[
                AuthSessionState::Authorized,
                AuthSessionState::Expired,
                AuthSessionState::Revoked,
            ],
        ),
        (&AuthSessionState::Expired, &[]),
        (&AuthSessionState::Revoked, &[]),
        (&AuthSessionState::Failed, &[]),
    ];

    /// Canonical wire name of the state (REST/MCP expose this string).
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Pending => "PENDING",
            Self::AwaitingConfirmation => "AWAITING_CONFIRMATION",
            Self::Authorized => "AUTHORIZED",
            Self::Expired => "EXPIRED",
            Self::Revoked => "REVOKED",
            Self::Failed => "FAILED",
            Self::TakenOver => "TAKEN_OVER",
        }
    }

    /// Parses the canonical wire name (case-sensitive).
    pub fn parse(raw: &str) -> DomainResult<Self> {
        match raw {
            "PENDING" => Ok(Self::Pending),
            "AWAITING_CONFIRMATION" => Ok(Self::AwaitingConfirmation),
            "AUTHORIZED" => Ok(Self::Authorized),
            "EXPIRED" => Ok(Self::Expired),
            "REVOKED" => Ok(Self::Revoked),
            "FAILED" => Ok(Self::Failed),
            "TAKEN_OVER" => Ok(Self::TakenOver),
            other => Err(DomainError::InvalidStateName {
                machine: "auth session",
                raw: other.to_owned(),
            }),
        }
    }

    /// Whether the state is terminal (no further transitions).
    pub fn is_terminal(self) -> bool {
        self.next_states().is_empty()
    }

    fn next_states(self) -> &'static [AuthSessionState] {
        Self::TRANSITIONS
            .iter()
            .find(|(from, _)| **from == self)
            .map(|(_, to)| *to)
            .unwrap_or(&[])
    }

    /// Applies one transition, enforcing the table.
    pub fn transition_to(self, next: AuthSessionState) -> DomainResult<AuthSessionState> {
        if self.next_states().contains(&next) {
            Ok(next)
        } else {
            Err(DomainError::InvalidTransition {
                machine: "auth session",
                from: self.as_str(),
                to: next.as_str(),
            })
        }
    }
}

impl std::fmt::Display for AuthSessionState {
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

    #[test]
    fn auth_session_happy_path_is_legal() {
        let mut state = AuthSessionState::Pending;
        state = state
            .transition_to(AuthSessionState::AwaitingConfirmation)
            .unwrap();
        state = state.transition_to(AuthSessionState::Authorized).unwrap();
        assert!(!state.is_terminal());
        state = state.transition_to(AuthSessionState::Expired).unwrap();
        assert!(state.is_terminal());
    }

    #[test]
    fn auth_session_pending_and_confirming_can_fail_or_be_revoked() {
        for start in [
            AuthSessionState::Pending,
            AuthSessionState::AwaitingConfirmation,
        ] {
            assert!(start.transition_to(AuthSessionState::Failed).is_ok());
            assert!(start.transition_to(AuthSessionState::Revoked).is_ok());
            assert!(start.transition_to(AuthSessionState::Expired).is_ok());
        }
        // an authorized session can only expire, be revoked, or be taken
        // over — never "fail" backwards
        assert!(
            AuthSessionState::Authorized
                .transition_to(AuthSessionState::Failed)
                .is_err()
        );
        assert_eq!(
            AuthSessionState::Authorized.transition_to(AuthSessionState::TakenOver),
            Ok(AuthSessionState::TakenOver)
        );
        assert_eq!(
            AuthSessionState::TakenOver.transition_to(AuthSessionState::Authorized),
            Ok(AuthSessionState::Authorized)
        );
        // a takeover cannot start from a login that never completed
        assert!(
            AuthSessionState::Pending
                .transition_to(AuthSessionState::TakenOver)
                .is_err()
        );
    }

    #[test]
    fn auth_session_terminal_states_have_no_exits() {
        for terminal in [
            AuthSessionState::Expired,
            AuthSessionState::Revoked,
            AuthSessionState::Failed,
        ] {
            assert!(terminal.is_terminal());
            for next in [
                AuthSessionState::Pending,
                AuthSessionState::AwaitingConfirmation,
                AuthSessionState::Authorized,
                AuthSessionState::Expired,
                AuthSessionState::Revoked,
                AuthSessionState::Failed,
            ] {
                assert_eq!(
                    terminal.transition_to(next),
                    Err(DomainError::InvalidTransition {
                        machine: "auth session",
                        from: terminal.as_str(),
                        to: next.as_str(),
                    })
                );
            }
        }
    }

    #[test]
    fn auth_session_wire_names_roundtrip() {
        for state in [
            AuthSessionState::Pending,
            AuthSessionState::AwaitingConfirmation,
            AuthSessionState::Authorized,
            AuthSessionState::Expired,
            AuthSessionState::Revoked,
            AuthSessionState::Failed,
            AuthSessionState::TakenOver,
        ] {
            assert_eq!(AuthSessionState::parse(state.as_str()), Ok(state));
            assert_eq!(state.to_string(), state.as_str());
        }
        let err = AuthSessionState::parse("authorized").unwrap_err();
        assert!(err.to_string().contains("auth session"));
    }
}
