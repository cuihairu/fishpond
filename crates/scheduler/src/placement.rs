//! Capacity- and affinity-aware account placement (F013).
//!
//! Pure decision logic with no I/O: given the tenant's workers and
//! their live lease counts, [`choose`] picks which worker should hold
//! a placement. The scheduler service layer feeds it fresh snapshots
//! and executes the chosen acquire through the storage CAS.
//!
//! Hard gates, in order: the worker must be *live* (heartbeat within
//! the policy's staleness window — the complement of
//! [`Store::lost_workers`][crate::Store]), must not be *draining*
//! (drain refuses new placements and migrates leases away), and must
//! *cover* the required capabilities (affinity). A worker at its
//! account capacity is rejected for a *new* placement — but the worker
//! that already holds the account keeps it, because capacity gates how
//! many accounts a worker holds, not which ones. Among eligible
//! workers the least-loaded wins; ties break on registration order and
//! then id, so a given snapshot always resolves deterministically.

use fishpond_domain::{UnixMillis, WorkerId};

/// Placement knobs for a tenant (F013).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlacementPolicy {
    /// Maximum accounts one worker may hold.
    pub max_accounts_per_worker: usize,
    /// How late a heartbeat may be before the worker counts as lost;
    /// a worker with `last_heartbeat_at <= now - after` is not live.
    pub heartbeat_stale_after_ms: i64,
}

impl PlacementPolicy {
    /// Builds a policy, rejecting non-positive knobs.
    pub fn new(max_accounts_per_worker: usize, heartbeat_stale_after_ms: i64) -> Option<Self> {
        if max_accounts_per_worker == 0 || heartbeat_stale_after_ms <= 0 {
            return None;
        }
        Some(Self {
            max_accounts_per_worker,
            heartbeat_stale_after_ms,
        })
    }
}

/// A worker snapshot the placement sees.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkerView {
    /// Worker id.
    pub id: WorkerId,
    /// Declared capabilities (affinity requirement vocabulary).
    pub capabilities: Vec<String>,
    /// Whether the worker is draining (refuses new placements).
    pub draining: bool,
    /// Last heartbeat instant.
    pub last_heartbeat_at: UnixMillis,
    /// First registration instant (tie-break).
    pub registered_at: UnixMillis,
    /// Live leases currently held (capacity accounting).
    pub lease_count: usize,
}

/// Chooses the worker that should hold `account` right now, or `None`
/// when no worker is eligible. `required` are the capabilities the
/// placement needs (empty when any engine may run it); `prefer` is the
/// account's current holder and wins over load when it still passes the
/// gates — stickiness keeps accounts from bouncing between equally
/// idle workers.
pub fn choose<'a>(
    workers: &'a [WorkerView],
    required: &[String],
    now: UnixMillis,
    policy: &PlacementPolicy,
    prefer: Option<&WorkerId>,
) -> Option<&'a WorkerView> {
    let heartbeat_cutoff_ms = now
        .as_millis()
        .saturating_sub(policy.heartbeat_stale_after_ms);
    let eligible = |w: &&WorkerView| {
        !w.draining
            && w.last_heartbeat_at.as_millis() > heartbeat_cutoff_ms
            && required
                .iter()
                .all(|cap| w.capabilities.iter().any(|c| c == cap))
    };
    // Stickiness: the current holder keeps its account even at
    // capacity — shedding it is what drain/takeover are for, not
    // placement.
    if let Some(found) = prefer.and_then(|preferred| workers.iter().find(|w| w.id == *preferred))
        && eligible(&found)
    {
        return Some(found);
    }
    let mut best: Option<&WorkerView> = None;
    for worker in workers {
        if !eligible(&worker) {
            continue;
        }
        // Capacity gates new placements only.
        if worker.lease_count >= policy.max_accounts_per_worker {
            continue;
        }
        best = match best {
            None => Some(worker),
            Some(current) => {
                if better_than(worker, current) {
                    Some(worker)
                } else {
                    Some(current)
                }
            }
        };
    }
    best
}

/// Strict "should win over" comparison: fewer live leases, then older
/// registration, then lower id — total order, deterministic.
fn better_than(candidate: &WorkerView, incumbent: &WorkerView) -> bool {
    (
        candidate.lease_count,
        candidate.registered_at.as_millis(),
        candidate.id.as_str(),
    ) < (
        incumbent.lease_count,
        incumbent.registered_at.as_millis(),
        incumbent.id.as_str(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ts(millis: i64) -> UnixMillis {
        UnixMillis::from_millis(millis).expect("non-negative millis")
    }

    fn worker(tag: &str, n: u32) -> WorkerView {
        WorkerView {
            id: WorkerId::parse(format!("wrk-{tag}-{n}")).expect("id"),
            capabilities: vec!["browser".to_owned()],
            draining: false,
            last_heartbeat_at: ts(10_000),
            registered_at: ts(1_000 + n as i64),
            lease_count: 0,
        }
    }

    fn policy(max: usize) -> PlacementPolicy {
        PlacementPolicy::new(max, 5_000).expect("valid policy")
    }

    #[test]
    fn capacity_rejects_a_full_worker_for_new_placements() {
        let a = {
            let mut w = worker("cap", 1);
            w.lease_count = 2;
            w
        };
        let b = {
            let mut w = worker("cap", 2);
            w.lease_count = 1;
            w
        };
        let workers = [a, b];
        let policy = policy(2);
        let chosen = choose(&workers, &[], ts(10_000), &policy, None).expect("b fits");
        assert_eq!(chosen.id.as_str(), "wrk-cap-2");
    }

    #[test]
    fn the_current_holder_keeps_the_account_even_at_capacity() {
        let full = {
            let mut w = worker("stick", 1);
            w.lease_count = 2;
            w
        };
        let idle = {
            let mut w = worker("stick", 2);
            w.lease_count = 0;
            w
        };
        let workers = [full, idle];
        let policy = policy(2);
        let preferred = WorkerId::parse("wrk-stick-1").expect("id");
        let chosen = choose(&workers, &[], ts(10_000), &policy, Some(&preferred)).expect("stick");
        assert_eq!(chosen.id, preferred);
    }

    #[test]
    fn a_draining_worker_is_never_chosen_even_when_it_is_the_holder() {
        let draining = {
            let mut w = worker("drain", 1);
            w.draining = true;
            w.lease_count = 1;
            w
        };
        let fresh = worker("drain", 2);
        let workers = [draining, fresh];
        let policy = policy(4);
        // Drain refuses new placements: the fresh worker wins.
        let chosen = choose(&workers, &[], ts(10_000), &policy, None).expect("fresh wins");
        assert_eq!(chosen.id.as_str(), "wrk-drain-2");
        // And stickiness cannot resurrect a draining holder: the
        // account is shed from it on the next placement pass and falls
        // to the fresh worker instead.
        let preferred = WorkerId::parse("wrk-drain-1").expect("id");
        let shed = choose(&workers, &[], ts(10_000), &policy, Some(&preferred))
            .expect("account moves off the draining holder");
        assert_eq!(
            shed.id.as_str(),
            "wrk-drain-2",
            "a draining holder sheds its account on placement"
        );
    }

    #[test]
    fn lost_workers_are_excluded_and_alive_ones_included() {
        let alive = worker("live", 1);
        let lost = {
            let mut w = worker("live", 2);
            w.last_heartbeat_at = ts(4_000); // now - 5_000 = 5_000 cutoff; 4_000 <= 5_000
            w
        };
        let workers = [alive, lost];
        let policy = policy(4);
        let chosen = choose(&workers, &[], ts(10_000), &policy, None).expect("alive");
        assert_eq!(chosen.id.as_str(), "wrk-live-1");
        // The cutoff is exclusive: a heartbeat exactly at the edge is
        // already lost.
        let edge = {
            let mut w = worker("live", 3);
            w.last_heartbeat_at = ts(5_000);
            w
        };
        let only_edge = [edge];
        assert!(
            choose(&only_edge, &[], ts(10_000), &policy, None).is_none(),
            "heartbeat at the cutoff counts as lost"
        );
    }

    #[test]
    fn required_capabilities_must_be_covered() {
        let browser = worker("aff", 1);
        let both = {
            let mut w = worker("aff", 2);
            w.capabilities = vec!["browser".to_owned(), "xianyu".to_owned()];
            w
        };
        let workers = [browser, both];
        let policy = policy(4);
        let required = vec!["xianyu".to_owned()];
        let chosen = choose(&workers, &required, ts(10_000), &policy, None).expect("both covers");
        assert_eq!(chosen.id.as_str(), "wrk-aff-2");
    }

    #[test]
    fn least_loaded_wins_with_deterministic_tie_breaks() {
        let light = {
            let mut w = worker("load", 1);
            w.lease_count = 0;
            w
        };
        let heavy = {
            let mut w = worker("load", 2);
            w.lease_count = 3;
            w
        };
        let by_load = [heavy, light];
        let policy = policy(8);
        let chosen = choose(&by_load, &[], ts(10_000), &policy, None).expect("someone");
        assert_eq!(chosen.id.as_str(), "wrk-load-1");
        // Equal load → older registration (lower registered_at) wins.
        let older = {
            let mut w = worker("load", 3);
            w.registered_at = ts(100);
            w
        };
        let newer = {
            let mut w = worker("load", 4);
            w.registered_at = ts(900);
            w
        };
        let by_registration = [newer, older];
        let chosen = choose(&by_registration, &[], ts(10_000), &policy, None).expect("someone");
        assert_eq!(chosen.id.as_str(), "wrk-load-3");
        // Same registration instant → lower id wins.
        let id_higher = {
            let mut w = worker("load", 6);
            w.registered_at = ts(500);
            w
        };
        let id_lower = {
            let mut w = worker("load", 5);
            w.registered_at = ts(500);
            w
        };
        let by_id = [id_higher, id_lower];
        let chosen = choose(&by_id, &[], ts(10_000), &policy, None).expect("someone");
        assert_eq!(chosen.id.as_str(), "wrk-load-5");
    }

    #[test]
    fn no_eligible_worker_yields_none() {
        let policy = policy(4);
        let empty: [WorkerView; 0] = [];
        assert!(choose(&empty, &[], ts(10_000), &policy, None).is_none());
        // Required capability nobody has.
        let w2 = worker("none", 2);
        let workers = [w2];
        let required = vec!["sftp".to_owned()];
        assert!(choose(&workers, &required, ts(10_000), &policy, None).is_none());
    }

    #[test]
    fn policy_validation_rejects_degenerate_inputs() {
        assert!(PlacementPolicy::new(0, 5_000).is_none());
        assert!(PlacementPolicy::new(4, 0).is_none());
        assert!(PlacementPolicy::new(4, -1).is_none());
    }
}
