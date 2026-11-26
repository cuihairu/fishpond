//! Reconcile service (F015): the control loop that resolves uncertainty.
//!
//! Two kinds of uncertainty need human-grade honesty, and both are
//! resolved here:
//!
//! * **Unverified accounts** — an account whose observed state could
//!   not be verified (`UNKNOWN`, or any probe that cannot confirm) is
//!   never silently mapped to online/offline. [`Reconciler::probe_account`]
//!   runs one injected [`AccountProbe`] and applies the outcome through
//!   the domain state machine: a confirmed observation transitions the
//!   account, an unverifiable one moves it to `UNKNOWN` (note required)
//!   or refreshes that note, and a disabled account is skipped without
//!   probing — it is administratively off, so probing it would lie
//!   about its reachability.
//! * **Expired attempts** — F014's deadline scan is deliberately a
//!   read; [`Reconciler::reap_deadlines`] is the caller that closes
//!   each expired in-flight attempt as `TimedOut`, returning the
//!   operation to `PENDING` for a later attempt. A worker that closed
//!   the attempt concurrently wins its CAS and the reaper records the
//!   attempt as contested instead of fighting.
//!
//! **Manual verification** (人工核对) is the operator's entry into the
//! same loop: [`Reconciler::confirm_manual`] applies a human decision
//! through the same state machine, and every step — automatic or
//! manual — appends an [`fishpond_storage::ObservationRecord`] so the
//! accounts row's *why* stays auditable (the live row cannot carry a
//! note on a confirmed state by domain invariant).
//!
//! The probe is injected, not built in: real upstream connections are
//! gated behind F032/F033 authorization, so CI exercises this service
//! with mock probes only (§7.3: Mock is the only CI target).

use fishpond_domain::{AccountId, AccountState, TenantId, UnixMillis};
use fishpond_storage::{
    NewObservation, ObservationOutcome, ObservationSource, StorageError, Store,
};

/// What one upstream probe concluded about an account.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProbeOutcome {
    /// The probe verified the account is in this confirmed state
    /// (`ONLINE` or `OFFLINE` — `UNKNOWN` and `DISABLED` are not
    /// probe-verifiable conclusions).
    Confirmed(AccountState),
    /// The probe could not verify the state; the reason is recorded on
    /// the account (`UNKNOWN` note) and in the audit trail.
    Unverifiable {
        /// Why the state could not be verified (never contains secrets).
        reason: String,
    },
}

/// One upstream probe for one account.
///
/// Real implementations wrap a provider connection (authorized in
/// F032+); tests inject mocks. The trait is async because a probe is
/// an upstream call.
#[async_trait::async_trait]
pub trait AccountProbe: Send + Sync {
    /// Probes one account once.
    async fn probe(&self, account: &AccountId) -> ProbeOutcome;
}

/// A reconcile failure.
#[derive(Debug)]
pub enum ReconcileError {
    /// The account does not exist or is not visible to the tenant.
    NotFound {
        /// What was not found, e.g. `"account"`.
        what: &'static str,
        /// The requested id.
        id: String,
    },
    /// The caller submitted arguments that violate an invariant
    /// (confirming to `UNKNOWN`, empty operator, ...).
    Invalid {
        /// What was invalid.
        what: &'static str,
        /// Why (never contains secrets).
        reason: String,
    },
    /// The change would violate a state machine or coordination rule.
    Conflict {
        /// What is in conflict.
        what: &'static str,
        /// The id of the conflicting resource.
        id: String,
        /// Why the change conflicts with current state.
        reason: String,
    },
    /// Persistence failed.
    Storage(StorageError),
}

impl std::fmt::Display for ReconcileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound { what, id } => write!(f, "{what} not found: {id}"),
            Self::Invalid { what, reason } => write!(f, "invalid {what}: {reason}"),
            Self::Conflict { what, id, reason } => {
                write!(f, "conflict on {what} {id}: {reason}")
            }
            Self::Storage(e) => write!(f, "storage: {e}"),
        }
    }
}

impl std::error::Error for ReconcileError {}

impl From<StorageError> for ReconcileError {
    fn from(e: StorageError) -> Self {
        match e {
            StorageError::NotFound { what, id } => Self::NotFound { what, id },
            other => Self::Storage(other),
        }
    }
}

impl From<fishpond_domain::DomainError> for ReconcileError {
    fn from(e: fishpond_domain::DomainError) -> Self {
        Self::Conflict {
            what: "state machine",
            id: "account".to_owned(),
            reason: e.to_string(),
        }
    }
}

/// Result alias for reconcile calls.
pub type ReconcileResult<T> = Result<T, ReconcileError>;

/// One reaped (closed as `TimedOut`) attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reaped {
    /// The owning operation (now back to `PENDING`).
    pub operation: fishpond_domain::OperationId,
    /// The attempt closed as timed out.
    pub attempt: fishpond_domain::AttemptId,
}

/// One attempt the reaper found expired but could not close because
/// its worker completed it concurrently (the worker's CAS won).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Contested {
    /// The owning operation.
    pub operation: fishpond_domain::OperationId,
    /// The attempt that was closed by its holder first.
    pub attempt: fishpond_domain::AttemptId,
}

/// The result of one deadline-reaping pass.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ReapReport {
    /// Attempts closed as `TimedOut`; their operations are `PENDING`.
    pub reaped: Vec<Reaped>,
    /// Attempts whose holder closed them while the reaper was running.
    pub contested: Vec<Contested>,
}

/// What one account reconcile step did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReconcileStep {
    /// The probe's confirmed state was applied (`from` → `to`).
    Applied {
        /// The state before the observation.
        from: AccountState,
        /// The state applied.
        to: AccountState,
    },
    /// The account already showed the observed state; no transition.
    /// For a still-unverifiable `UNKNOWN` account the note was
    /// refreshed to the newest probe reason.
    Unchanged(AccountState),
    /// The account is administratively disabled; it was not probed.
    SkippedDisabled,
}

/// The reconcile service for one store.
///
/// Cheap to clone; all methods take the tenant explicitly and run
/// under that tenant's row-level security.
#[derive(Clone)]
pub struct Reconciler {
    store: Store,
    max_reap_per_pass: usize,
}

impl Reconciler {
    /// Builds the service; `max_reap_per_pass` bounds each reaping
    /// pass so one burst of expired attempts cannot monopolize the
    /// loop (the next pass picks up the rest).
    pub fn new(store: Store, max_reap_per_pass: usize) -> Self {
        Self {
            store,
            max_reap_per_pass: max_reap_per_pass.max(1),
        }
    }

    /// The bounded per-pass reap limit.
    pub fn max_reap_per_pass(&self) -> usize {
        self.max_reap_per_pass
    }

    /// Runs one deadline-reaping pass at `now`: reads the expired
    /// in-flight attempts and closes them (see [`Self::reap_expired`]).
    /// Idempotent: a second pass finds only what became expired since.
    pub async fn reap_deadlines(
        &self,
        tenant: &TenantId,
        now: UnixMillis,
    ) -> ReconcileResult<ReapReport> {
        let expired = self.store.expired_attempts(tenant, now).await?;
        self.reap_expired(tenant, expired, now).await
    }

    /// Closes up to [`Self::max_reap_per_pass`] of the given expired
    /// in-flight attempts as `TimedOut` (their operations return to
    /// `PENDING`) and reports the ones whose worker closed them
    /// concurrently (the worker's completion CAS wins; the reaper
    /// records the attempt as contested instead of fighting).
    ///
    /// Split from [`Self::reap_deadlines`] so a caller may hold the
    /// snapshot across an await, and so the worker-wins race is
    /// deterministically testable.
    pub async fn reap_expired(
        &self,
        tenant: &TenantId,
        expired: Vec<fishpond_storage::ExpiredAttempt>,
        now: UnixMillis,
    ) -> ReconcileResult<ReapReport> {
        let mut report = ReapReport::default();
        for expired in expired.into_iter().take(self.max_reap_per_pass) {
            let attempt = &expired.attempt;
            let Some(worker) = attempt.worker.as_ref() else {
                // An in-flight attempt always records its starter, but
                // a hand-inserted row without one cannot be closed by
                // the reaper (close is CAS'd to the holder). Skip it;
                // the next pass sees it again until someone acts.
                continue;
            };
            let operation = attempt.operation_id.clone();
            let attempt_id = attempt.attempt_id.clone();
            match self
                .store
                .close_attempt(
                    tenant,
                    &operation,
                    &attempt_id,
                    worker,
                    fishpond_storage::AttemptClose::TimedOut,
                    now,
                )
                .await
            {
                Ok(_) => report.reaped.push(Reaped {
                    operation,
                    attempt: attempt_id,
                }),
                Err(StorageError::Conflict { .. }) => report.contested.push(Contested {
                    operation,
                    attempt: attempt_id,
                }),
                Err(e) => return Err(e.into()),
            }
        }
        Ok(report)
    }

    /// Runs one probe-and-apply step for `account` at `at`: probes via
    /// `probe`, applies the outcome through the domain machine and
    /// appends the audit observation. Returns the step taken.
    pub async fn probe_account(
        &self,
        tenant: &TenantId,
        account: &AccountId,
        probe: &dyn AccountProbe,
        observation_id: &str,
        at: UnixMillis,
    ) -> ReconcileResult<ReconcileStep> {
        let current = self.store.get_account(tenant, account).await?;
        if current.state == AccountState::Disabled {
            self.record(
                tenant,
                account,
                ObservationSource::Reconcile,
                AccountState::Disabled,
                ObservationOutcome::Skipped,
                None,
                None,
                observation_id,
                at,
            )
            .await?;
            return Ok(ReconcileStep::SkippedDisabled);
        }
        match probe.probe(account).await {
            ProbeOutcome::Confirmed(state) => {
                if !matches!(state, AccountState::Online | AccountState::Offline) {
                    return Err(ReconcileError::Invalid {
                        what: "probe outcome",
                        reason: format!(
                            "probe confirmed {}, which is not a probe-verifiable state",
                            state.as_str()
                        ),
                    });
                }
                if state == current.state {
                    self.record(
                        tenant,
                        account,
                        ObservationSource::Reconcile,
                        state,
                        ObservationOutcome::Unchanged,
                        None,
                        None,
                        observation_id,
                        at,
                    )
                    .await?;
                    return Ok(ReconcileStep::Unchanged(current.state));
                }
                let applied = current.state.transition_to(state)?;
                self.store
                    .update_account_state(tenant, account, applied, None, at)
                    .await?;
                self.record(
                    tenant,
                    account,
                    ObservationSource::Reconcile,
                    applied,
                    ObservationOutcome::Applied,
                    None,
                    None,
                    observation_id,
                    at,
                )
                .await?;
                Ok(ReconcileStep::Applied {
                    from: current.state,
                    to: applied,
                })
            }
            ProbeOutcome::Unverifiable { reason } => {
                if reason.trim().is_empty() {
                    return Err(ReconcileError::Invalid {
                        what: "probe outcome",
                        reason: "unverifiable probe must say why".to_owned(),
                    });
                }
                if current.state == AccountState::Unknown {
                    // Same state: staying put is not a transition, but
                    // the note is refreshed to the newest reason.
                    self.store
                        .update_account_state(
                            tenant,
                            account,
                            AccountState::Unknown,
                            Some(reason.clone()),
                            at,
                        )
                        .await?;
                    self.record(
                        tenant,
                        account,
                        ObservationSource::Reconcile,
                        AccountState::Unknown,
                        ObservationOutcome::Unchanged,
                        Some(&reason),
                        None,
                        observation_id,
                        at,
                    )
                    .await?;
                    return Ok(ReconcileStep::Unchanged(AccountState::Unknown));
                }
                let applied = current.state.transition_to(AccountState::Unknown)?;
                self.store
                    .update_account_state(tenant, account, applied, Some(reason.clone()), at)
                    .await?;
                self.record(
                    tenant,
                    account,
                    ObservationSource::Reconcile,
                    AccountState::Unknown,
                    ObservationOutcome::Applied,
                    Some(&reason),
                    None,
                    observation_id,
                    at,
                )
                .await?;
                Ok(ReconcileStep::Applied {
                    from: current.state,
                    to: applied,
                })
            }
        }
    }

    /// Applies a human verification (人工核对) at `at`: `confirmed` must
    /// be a confirmed state (`ONLINE`/`OFFLINE` — a human asserting
    /// "unverified" is not a verification), `operator` names who decided,
    /// and `justification` (optional) is recorded on the audit trail —
    /// the live account row cannot carry it on a confirmed state by
    /// domain invariant. Confirming the state the account already has is
    /// an `Unchanged` verification, still audited.
    #[allow(clippy::too_many_arguments)]
    pub async fn confirm_manual(
        &self,
        tenant: &TenantId,
        account: &AccountId,
        confirmed: AccountState,
        operator: &str,
        justification: Option<&str>,
        observation_id: &str,
        at: UnixMillis,
    ) -> ReconcileResult<ReconcileStep> {
        if operator.trim().is_empty() {
            return Err(ReconcileError::Invalid {
                what: "operator",
                reason: "must name who verified".to_owned(),
            });
        }
        if !matches!(confirmed, AccountState::Online | AccountState::Offline) {
            return Err(ReconcileError::Invalid {
                what: "manual confirmation",
                reason: format!(
                    "{} is not a confirmed state; humans verify to ONLINE/OFFLINE only",
                    confirmed.as_str()
                ),
            });
        }
        let current = self.store.get_account(tenant, account).await?;
        if current.state == AccountState::Disabled {
            return Err(ReconcileError::Conflict {
                what: "manual confirmation",
                id: account.as_str().to_owned(),
                reason: "account is disabled (terminal); re-enable via a fresh lifecycle"
                    .to_owned(),
            });
        }
        if confirmed == current.state {
            self.record(
                tenant,
                account,
                ObservationSource::Manual,
                confirmed,
                ObservationOutcome::Unchanged,
                justification,
                Some(operator),
                observation_id,
                at,
            )
            .await?;
            return Ok(ReconcileStep::Unchanged(confirmed));
        }
        let applied = current.state.transition_to(confirmed)?;
        self.store
            .update_account_state(tenant, account, applied, None, at)
            .await?;
        self.record(
            tenant,
            account,
            ObservationSource::Manual,
            applied,
            ObservationOutcome::Applied,
            justification,
            Some(operator),
            observation_id,
            at,
        )
        .await?;
        Ok(ReconcileStep::Applied {
            from: current.state,
            to: applied,
        })
    }

    /// Appends one audit observation (storage errors pass through).
    #[allow(clippy::too_many_arguments)]
    async fn record(
        &self,
        tenant: &TenantId,
        account: &AccountId,
        source: ObservationSource,
        observed: AccountState,
        outcome: ObservationOutcome,
        note: Option<&str>,
        operator: Option<&str>,
        observation_id: &str,
        at: UnixMillis,
    ) -> ReconcileResult<()> {
        self.store
            .record_observation(&NewObservation {
                id: observation_id,
                tenant,
                account,
                source,
                observed: observed.as_str(),
                outcome,
                note,
                operator,
                at,
            })
            .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_display_is_secret_free_and_specific() {
        let not_found = ReconcileError::NotFound {
            what: "account",
            id: "acc-1".to_owned(),
        };
        assert_eq!(not_found.to_string(), "account not found: acc-1");
        let invalid = ReconcileError::Invalid {
            what: "operator",
            reason: "must name who verified".to_owned(),
        };
        assert_eq!(
            invalid.to_string(),
            "invalid operator: must name who verified"
        );
        let conflict = ReconcileError::Conflict {
            what: "manual confirmation",
            id: "acc-2".to_owned(),
            reason: "account is disabled (terminal)".to_owned(),
        };
        assert_eq!(
            conflict.to_string(),
            "conflict on manual confirmation acc-2: account is disabled (terminal)"
        );
    }

    #[test]
    fn storage_not_found_maps_to_not_found_and_other_to_storage() {
        let mapped = ReconcileError::from(StorageError::NotFound {
            what: "account",
            id: "acc-3".to_owned(),
        });
        assert!(matches!(mapped, ReconcileError::NotFound { .. }));
        let mapped = ReconcileError::from(StorageError::Invalid {
            what: "row",
            reason: "boom".to_owned(),
        });
        assert!(matches!(mapped, ReconcileError::Storage(_)));
    }

    #[test]
    fn domain_transition_error_becomes_a_conflict() {
        // DISABLED is terminal: any transition from it is illegal.
        let mapped = ReconcileError::from(
            AccountState::Disabled
                .transition_to(AccountState::Online)
                .expect_err("disabled has no exits"),
        );
        let ReconcileError::Conflict { reason, .. } = &mapped else {
            panic!("expected a conflict, got {mapped:?}");
        };
        assert!(reason.contains("illegal"), "names the transition: {reason}");
    }
}
