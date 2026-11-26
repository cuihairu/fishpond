//! F013 acceptance: the scheduler service — placement (capacity +
//! affinity + stickiness), drain with lease migration, and failure
//! takeover of lost workers.

mod common;

use common::{ENV, account, gated, tenant_record, ts, unique_entity, unique_tenant};
use fishpond_domain::{AccountId, AccountState, TenantId, WorkerId};
use fishpond_scheduler::{PlacementPolicy, Scheduler, SchedulerError};
use fishpond_storage::{NewWorker, Store};

fn policy() -> PlacementPolicy {
    PlacementPolicy::new(1, 5_000).expect("valid policy")
}

/// Registers `n` workers with fresh heartbeats at `now`; worker `i`
/// gets `["browser", "xianyu"]` when `xianyu` is set, else
/// `["browser"]`. Registration order = the list order.
async fn register_workers(
    store: &Store,
    tenant: &TenantId,
    tag: &str,
    specs: &[(u32, bool)],
    now: fishpond_domain::UnixMillis,
) -> Vec<WorkerId> {
    let mut ids = Vec::new();
    for (n, with_xianyu) in specs {
        let id = WorkerId::parse(unique_entity("wrk", &format!("{tag}-{n}"))).expect("worker id");
        let caps: &[&str] = if *with_xianyu {
            &["browser", "xianyu"]
        } else {
            &["browser"]
        };
        store
            .register_worker(&NewWorker {
                id: &id,
                tenant,
                name: &format!("node-{n}"),
                capabilities: caps,
                at: now,
            })
            .await
            .expect("register worker");
        ids.push(id);
    }
    ids
}

async fn seed_accounts(store: &Store, tenant: &TenantId, tag: &str, n: u32) -> Vec<AccountId> {
    let mut ids = Vec::new();
    for i in 1..=n {
        let id = AccountId::parse(unique_entity("acc", &format!("{tag}-{i}"))).expect("account id");
        store
            .upsert_account(&account(id.as_str(), tenant, AccountState::Online, None))
            .await
            .expect("account insert");
        ids.push(id);
    }
    ids
}

#[tokio::test]
async fn assign_respects_capacity_affinity_and_stickiness() {
    let Some(store) = gated().await else {
        eprintln!("skip: {ENV} is not set (CI has no database)");
        return;
    };
    let tenant = unique_tenant("sched-assign");
    store
        .upsert_tenant(&tenant_record(&tenant, "sched-assign"))
        .await
        .expect("tenant insert");
    // w1 first (older registration), w2 second — both cover xianyu.
    let workers = register_workers(
        &store,
        &tenant,
        "assign",
        &[(1, true), (2, true)],
        ts(9_000),
    )
    .await;
    let [w1, w2] = &workers[..] else {
        panic!("two workers registered");
    };
    let (w1, w2) = (w1.clone(), w2.clone());
    let scheduler = Scheduler::new(store.clone(), policy()); // max 1 per worker
    let accounts = seed_accounts(&store, &tenant, "assign", 4).await;
    let [a1, a2, a3, a4] = &accounts[..] else {
        panic!("four accounts seeded");
    };
    let (a1, a2, a3, a4) = (a1.clone(), a2.clone(), a3.clone(), a4.clone());
    let required = vec!["xianyu".to_owned()];

    // First placement: both workers eligible and empty; the older
    // registration (w1) wins the deterministic tie-break.
    let first = scheduler
        .assign(&tenant, &a1, &required, None, 60_000, ts(10_000))
        .await
        .expect("first placement");
    assert_eq!(first.holder, w1);
    assert_eq!(first.lease.generation, 1);

    // w1 is now at capacity 1 → the second account lands on w2.
    let second = scheduler
        .assign(&tenant, &a2, &required, None, 60_000, ts(10_100))
        .await
        .expect("second placement");
    assert_eq!(second.holder, w2);

    // Both at capacity → nothing eligible.
    let full = scheduler
        .assign(&tenant, &a3, &required, None, 60_000, ts(10_200))
        .await
        .expect_err("capacity exhausted");
    assert!(
        matches!(full, SchedulerError::NoEligibleWorker { .. }),
        "expected NoEligibleWorker, got {full}"
    );

    // Re-placing a1 (its holder is w1) keeps it on w1 despite capacity:
    // stickiness, and the CAS re-extends the term without a new
    // generation.
    let current = store
        .get_lease(&tenant, &a1)
        .await
        .expect("get")
        .expect("a1 leased");
    let restuck = scheduler
        .assign(
            &tenant,
            &a1,
            &required,
            Some(&current.holder),
            120_000,
            ts(10_300),
        )
        .await
        .expect("restick");
    assert_eq!(restuck.holder, w1);
    assert_eq!(
        restuck.lease.generation, 1,
        "re-extension does not bump generation"
    );
    assert_eq!(restuck.lease.expires_at, ts(130_300));

    // Affinity: a4 needs a capability nobody has.
    let mismatched = scheduler
        .assign(&tenant, &a4, &["sftp".to_owned()], None, 60_000, ts(10_400))
        .await
        .expect_err("affinity unmatched");
    assert!(matches!(
        mismatched,
        SchedulerError::NoEligibleWorker { .. }
    ));
}

#[tokio::test]
async fn drain_stops_placement_and_migrates_live_leases() {
    let Some(store) = gated().await else {
        eprintln!("skip: {ENV} is not set (CI has no database)");
        return;
    };
    let tenant = unique_tenant("sched-drain");
    store
        .upsert_tenant(&tenant_record(&tenant, "sched-drain"))
        .await
        .expect("tenant insert");
    let workers =
        register_workers(&store, &tenant, "drain", &[(1, true), (2, true)], ts(9_000)).await;
    let [w1, w2] = &workers[..] else {
        panic!("two workers registered");
    };
    let (w1, w2) = (w1.clone(), w2.clone());
    // Capacity 4: w2 can absorb everything w1 holds.
    let scheduler = Scheduler::new(
        store.clone(),
        PlacementPolicy::new(4, 5_000).expect("valid"),
    );
    let accounts = seed_accounts(&store, &tenant, "drain", 3).await;
    let [a1, a2, a3] = &accounts[..] else {
        panic!("three accounts seeded");
    };
    let (a1, a2, a3) = (a1.clone(), a2.clone(), a3.clone());
    // The first placement lands on the older-registered worker; the
    // second is pinned there with stickiness so w1 holds both.
    scheduler
        .assign(&tenant, &a1, &[], None, 60_000, ts(10_000))
        .await
        .expect("initial placement on w1");
    scheduler
        .assign(&tenant, &a2, &[], Some(&w1), 60_000, ts(10_000))
        .await
        .expect("pinned placement on w1");
    assert_eq!(
        store
            .get_lease(&tenant, &a1)
            .await
            .expect("get")
            .expect("leased")
            .holder,
        w1,
        "older registration takes the first account"
    );
    assert_eq!(
        store
            .get_lease(&tenant, &a2)
            .await
            .expect("get")
            .expect("leased")
            .holder,
        w1,
        "stickiness keeps the second account on w1"
    );

    // Drain w1: mark, force-expire, migrate to w2 with a risen
    // generation.
    let report = scheduler
        .drain(&tenant, &w1, 60_000, ts(11_000))
        .await
        .expect("drain");
    assert!(!report.already_draining);
    assert_eq!(report.migration.moved.len(), 2, "both accounts migrated");
    assert!(report.migration.contested.is_empty());
    assert!(report.migration.unassigned.is_empty());
    for (account, holder, generation) in &report.migration.moved {
        assert_eq!(holder, &w2, "w2 is the only healthy target");
        assert_eq!(*generation, 2, "migration is a steal: generation rises");
        let lease = store
            .get_lease(&tenant, account)
            .await
            .expect("get")
            .expect("leased");
        assert_eq!(&lease.holder, holder);
        assert_eq!(
            lease.expires_at,
            ts(71_000),
            "new term from the migration instant"
        );
    }

    // w1 is draining: new placements never return to it, and a second
    // drain reports idempotence with nothing left to move.
    let after = scheduler
        .assign(&tenant, &a3, &[], None, 60_000, ts(11_100))
        .await
        .expect("placement after drain");
    assert_eq!(after.holder, w2, "drained worker is skipped");
    let again = scheduler
        .drain(&tenant, &w1, 60_000, ts(11_200))
        .await
        .expect("second drain");
    assert!(again.already_draining);
    assert!(again.migration.moved.is_empty());

    // Draining the last healthy worker leaves the accounts unassigned,
    // not dropped: they are still there, just unheld.
    let last = scheduler
        .drain(&tenant, &w2, 60_000, ts(11_300))
        .await
        .expect("drain w2");
    assert_eq!(
        last.migration.unassigned.len(),
        3,
        "no healthy worker remains"
    );
    assert!(last.migration.moved.is_empty());
    for account in [&a1, &a2, &a3] {
        let lease = store.get_lease(&tenant, account).await.expect("get");
        assert!(
            lease.is_none() || lease.expect("row").expires_at <= ts(11_300),
            "cut or gone, never silently live on a drained worker"
        );
    }
}

#[tokio::test]
async fn takeover_moves_lost_workers_accounts_to_healthy_workers() {
    let Some(store) = gated().await else {
        eprintln!("skip: {ENV} is not set (CI has no database)");
        return;
    };
    let tenant = unique_tenant("sched-takeover");
    store
        .upsert_tenant(&tenant_record(&tenant, "sched-takeover"))
        .await
        .expect("tenant insert");
    let workers = register_workers(
        &store,
        &tenant,
        "takeover",
        &[(1, true), (2, true)],
        ts(9_000),
    )
    .await;
    let [w1, w2] = &workers[..] else {
        panic!("two workers registered");
    };
    let (w1, w2) = (w1.clone(), w2.clone());
    let scheduler = Scheduler::new(
        store.clone(),
        PlacementPolicy::new(4, 5_000).expect("valid"),
    );
    let accounts = seed_accounts(&store, &tenant, "takeover", 2).await;
    let [a1, a2] = &accounts[..] else {
        panic!("two accounts seeded");
    };
    let (a1, a2) = (a1.clone(), a2.clone());
    scheduler
        .assign(&tenant, &a1, &[], None, 60_000, ts(10_000))
        .await
        .expect("initial placement");
    scheduler
        .assign(&tenant, &a2, &[], Some(&w1), 60_000, ts(10_000))
        .await
        .expect("pinned placement");
    assert_eq!(
        store
            .get_lease(&tenant, &a2)
            .await
            .expect("get")
            .expect("leased")
            .holder,
        w1,
        "both accounts sit on w1"
    );

    // w1 dies: its heartbeat goes stale while its leases are still
    // nominally live (term until 70_000 > now 16_000). Takeover cuts
    // and moves them to w2.
    store
        .heartbeat_worker(&tenant, &w1, ts(4_000))
        .await
        .expect("stale w1");
    // w2 keeps beating.
    store
        .heartbeat_worker(&tenant, &w2, ts(15_000))
        .await
        .expect("fresh w2");
    let report = scheduler
        .take_over_lost(&tenant, 5_000, 60_000, ts(16_000))
        .await
        .expect("takeover");
    assert_eq!(
        report.migration.sources,
        vec![w1.clone()],
        "w1 is the lost worker"
    );
    assert_eq!(report.migration.moved.len(), 2);
    assert!(report.migration.unassigned.is_empty());
    for (account, holder, generation) in &report.migration.moved {
        assert_eq!(holder, &w2);
        assert_eq!(*generation, 2, "stolen term carries a risen generation");
        let lease = store
            .get_lease(&tenant, account)
            .await
            .expect("get")
            .expect("leased");
        assert_eq!(&lease.holder, holder);
    }

    // A second pass finds nothing live to move (idempotent).
    let again = scheduler
        .take_over_lost(&tenant, 5_000, 60_000, ts(16_500))
        .await
        .expect("second takeover");
    assert!(again.migration.moved.is_empty());

    // Lost and taken-over workers do not receive new placements.
    let fresh = scheduler
        .assign(&tenant, &a1, &[], None, 60_000, ts(17_000))
        .await
        .expect("re-place a1");
    assert_eq!(fresh.holder, w2, "the lost worker is never chosen");
}
