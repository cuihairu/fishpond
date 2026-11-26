//! Circuit breaker (F016): one circuit per tenant+platform, closed,
//! open or half-open.
//!
//! A circuit guards a *route*, not a caller: when consecutive upstream
//! failures cross the threshold, the circuit opens and every call for
//! that tenant+platform is short-circuited (`CircuitOpen`) until the
//! cooldown passes — then one probe call is allowed (half-open). The
//! probe's outcome decides: success closes the circuit, failure
//! reopens it for another cooldown. Caller faults never trip the
//! circuit (see [`crate::classify`]).
//!
//! Failure counting is *consecutive*: any success resets the streak,
//! so a provider that intermittently hiccups stays closed while a
//! persistently failing one opens fast. Time enters only through the
//! explicit `now` argument, so the state machine is fully
//! deterministic and unit-testable.
//!
//! Circuits live in memory: they are runtime health cache, not
//! durable state — a router restart starts everything closed, which
//! is the honest default (the first real failure re-opens a truly
//! dead route quickly).

use std::collections::HashMap;
use std::sync::Mutex;

use crate::error::RouteError;

/// Breaker thresholds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BreakerPolicy {
    /// Consecutive upstream failures that open the circuit.
    pub failure_threshold: u32,
    /// How long an open circuit stays open before allowing one probe
    /// (milliseconds).
    pub cooldown_ms: i64,
}

impl BreakerPolicy {
    /// Builds a policy, rejecting a zero threshold or non-positive
    /// cooldown (a breaker that opens on the first failure or never
    /// probes is a lock, not a breaker).
    pub fn new(failure_threshold: u32, cooldown_ms: i64) -> Option<Self> {
        if failure_threshold == 0 || cooldown_ms <= 0 {
            None
        } else {
            Some(Self {
                failure_threshold,
                cooldown_ms,
            })
        }
    }
}

/// The state of one circuit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BreakerState {
    /// Calls pass; consecutive failures accumulate.
    Closed,
    /// Calls are rejected until the cooldown passes.
    Open,
    /// The cooldown passed and one probe call is allowed; its outcome
    /// closes or reopens the circuit.
    HalfOpen,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Entry {
    state: BreakerState,
    consecutive_failures: u32,
    opened_at: i64,
}

/// The circuit breaker over a set of circuits.
pub struct CircuitBreaker {
    policy: BreakerPolicy,
    circuits: Mutex<HashMap<String, Entry>>,
}

impl CircuitBreaker {
    /// Builds a breaker with `policy`.
    pub fn new(policy: BreakerPolicy) -> Self {
        Self {
            policy,
            circuits: Mutex::new(HashMap::new()),
        }
    }

    /// The configured policy.
    pub fn policy(&self) -> &BreakerPolicy {
        &self.policy
    }

    /// Gates one call on `key`'s circuit at `now`. Returns the
    /// observed state when the call may proceed; [`RouteError::CircuitOpen`]
    /// when it may not. An open circuit whose cooldown has passed
    /// transitions to half-open *for this call* — the probe.
    pub fn check(&self, key: &str, now: i64) -> Result<BreakerState, RouteError> {
        let mut circuits = self.circuits.lock().expect("breaker lock");
        let entry = circuits.entry(key.to_owned()).or_insert(Entry {
            state: BreakerState::Closed,
            consecutive_failures: 0,
            opened_at: 0,
        });
        match entry.state {
            BreakerState::Closed | BreakerState::HalfOpen => Ok(entry.state),
            BreakerState::Open => {
                let elapsed = now.saturating_sub(entry.opened_at);
                if elapsed < self.policy.cooldown_ms {
                    Err(RouteError::CircuitOpen {
                        key: key.to_owned(),
                        retry_after_ms: self.policy.cooldown_ms - elapsed,
                    })
                } else {
                    // Cooldown served: this call is the probe.
                    entry.state = BreakerState::HalfOpen;
                    Ok(BreakerState::HalfOpen)
                }
            }
        }
    }

    /// Records one successful call: closes the circuit and resets the
    /// failure streak (a probe success re-closes; a closed-circuit
    /// success keeps it closed).
    pub fn record_success(&self, key: &str) {
        let mut circuits = self.circuits.lock().expect("breaker lock");
        if let Some(entry) = circuits.get_mut(key) {
            entry.state = BreakerState::Closed;
            entry.consecutive_failures = 0;
        }
    }

    /// Records one failed upstream call at `now`. In a closed circuit
    /// the consecutive streak grows; at the threshold the circuit
    /// opens. In a half-open circuit the probe failed: reopen for a
    /// fresh cooldown. An already-open circuit stays open (its first
    /// open instant is kept, so the cooldown is not extended by
    /// rejected callers that somehow recorded a failure).
    pub fn record_failure(&self, key: &str, now: i64) {
        let mut circuits = self.circuits.lock().expect("breaker lock");
        let entry = circuits.entry(key.to_owned()).or_insert(Entry {
            state: BreakerState::Closed,
            consecutive_failures: 0,
            opened_at: 0,
        });
        match entry.state {
            BreakerState::Closed => {
                entry.consecutive_failures += 1;
                if entry.consecutive_failures >= self.policy.failure_threshold {
                    entry.state = BreakerState::Open;
                    entry.opened_at = now;
                }
            }
            BreakerState::HalfOpen => {
                entry.state = BreakerState::Open;
                entry.opened_at = now;
            }
            BreakerState::Open => {}
        }
    }

    /// The current state of `key`'s circuit (`Closed` when it has
    /// never tripped or been probed).
    pub fn state(&self, key: &str) -> BreakerState {
        self.circuits
            .lock()
            .expect("breaker lock")
            .get(key)
            .map(|entry| entry.state)
            .unwrap_or(BreakerState::Closed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn breaker() -> CircuitBreaker {
        CircuitBreaker::new(BreakerPolicy::new(3, 100).expect("valid policy"))
    }

    #[test]
    fn policy_rejects_degenerate_settings() {
        assert!(BreakerPolicy::new(0, 100).is_none(), "zero threshold");
        assert!(BreakerPolicy::new(3, 0).is_none(), "zero cooldown");
        assert!(BreakerPolicy::new(3, -5).is_none(), "negative cooldown");
        assert!(BreakerPolicy::new(1, 1).is_some());
    }

    #[test]
    fn opens_after_consecutive_failures_and_stays_open_through_cooldown() {
        let b = breaker();
        assert_eq!(b.check("t/p", 0), Ok(BreakerState::Closed));
        b.record_failure("t/p", 10);
        b.record_failure("t/p", 11);
        assert_eq!(b.state("t/p"), BreakerState::Closed, "below threshold");
        b.record_failure("t/p", 12);
        assert_eq!(b.state("t/p"), BreakerState::Open);

        // Inside the cooldown: rejected with a concrete retry hint.
        let err = b.check("t/p", 50).expect_err("open");
        let RouteError::CircuitOpen {
            key,
            retry_after_ms,
        } = err
        else {
            panic!("expected CircuitOpen, got {err}");
        };
        assert_eq!(key, "t/p");
        assert_eq!(
            retry_after_ms, 62,
            "opened at 12, checked at 50: 100-(50-12)"
        );

        // At the cooldown boundary the next check becomes the probe.
        assert_eq!(b.check("t/p", 112), Ok(BreakerState::HalfOpen));
    }

    #[test]
    fn a_success_resets_the_streak() {
        let b = breaker();
        b.record_failure("t/p", 1);
        b.record_failure("t/p", 2);
        b.record_success("t/p");
        b.record_failure("t/p", 3);
        b.record_failure("t/p", 4);
        assert_eq!(b.state("t/p"), BreakerState::Closed, "streak was reset");
        b.record_failure("t/p", 5);
        assert_eq!(b.state("t/p"), BreakerState::Open);
    }

    #[test]
    fn a_failed_probe_reopens_for_a_fresh_cooldown() {
        let b = breaker();
        for now in [1, 2, 3] {
            b.record_failure("t/p", now);
        }
        assert_eq!(b.state("t/p"), BreakerState::Open);
        // Cooldown passes; probe is allowed and fails.
        assert_eq!(b.check("t/p", 103), Ok(BreakerState::HalfOpen));
        b.record_failure("t/p", 104);
        assert_eq!(b.state("t/p"), BreakerState::Open);
        // The fresh cooldown counts from the probe failure (104), not
        // the original trip (3).
        assert!(b.check("t/p", 150).is_err());
        assert_eq!(b.check("t/p", 204), Ok(BreakerState::HalfOpen));
    }

    #[test]
    fn a_successful_probe_closes_the_circuit() {
        let b = breaker();
        for now in [1, 2, 3] {
            b.record_failure("t/p", now);
        }
        assert_eq!(b.check("t/p", 103), Ok(BreakerState::HalfOpen));
        b.record_success("t/p");
        assert_eq!(b.state("t/p"), BreakerState::Closed);
        // The streak restarts from zero: three fresh failures to trip.
        b.record_failure("t/p", 104);
        b.record_failure("t/p", 105);
        assert_eq!(b.state("t/p"), BreakerState::Closed);
        b.record_failure("t/p", 106);
        assert_eq!(b.state("t/p"), BreakerState::Open);
    }

    #[test]
    fn circuits_are_independent_per_key() {
        let b = breaker();
        for now in [1, 2, 3] {
            b.record_failure("t-a/p", now);
        }
        assert_eq!(b.state("t-a/p"), BreakerState::Open);
        assert_eq!(b.state("t-b/p"), BreakerState::Closed);
        assert_eq!(b.check("t-b/p", 2), Ok(BreakerState::Closed));
    }

    #[test]
    fn an_unseen_key_reports_closed() {
        let b = breaker();
        assert_eq!(b.state("never/touched"), BreakerState::Closed);
        assert_eq!(b.check("never/touched", 0), Ok(BreakerState::Closed));
    }
}
