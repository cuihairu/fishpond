//! F039 acceptance (fault injection): the failure paths an operator
//! actually sees, end to end over the real database —
//!
//! * a lease holder dies mid-operation: takeover recovers the account,
//!   the fencing generation rises, and the dead holder's next renew is
//!   refused by name; its abandoned attempt is reaped (timed out) and
//!   the operation returns to PENDING for a fresh holder, whose attempt
//!   number bumps;
//! * a drained node under full-capacity pressure refuses placements
//!   and recovers them on undrain.
//!
//! The operation-machine pieces (claim CAS, TimedOut requeue) are
//! unit-covered in storage; what is new here is the fault *chain* —
//! death, fencing, reaping, reclaim — proven as one path. Same skip
//! contract as the other scheduler suites.

mod common;

use common::{ENV, account, gated, tenant_record, ts, unique_entity, unique_tenant};
use fishpond_domain::{AccountId, AccountState, AttemptId, OperationId, TenantId, WorkerId};
use fishpond_scheduler::{PlacementPolicy, Scheduler, SchedulerError};
use fishpond_storage::{AttemptClose, NewOperation, NewWorker, Store};

async fn register(
    store: &Store,
    tenant: &TenantId,
    tag: &str,
    node: &str,
    at: fishpond_domain::UnixMillis,
) -> WorkerId {
    let id = WorkerId::parse(unique_entity("wrk", tag)).expect("worker id");
    store
        .register_worker(&NewWorker {
            id: &id,
            tenant,
            name: node,
            capabilities: &["browser"],
            at,
        })
        .await
        .expect("register worker");
    id
}

async fn seed_account(store: &Store, tenant: &TenantId, tag: &str) -> AccountId {
    let id = AccountId::parse(unique_entity("acc", tag)).expect("account id");
    store
        .upsert_account(&account(id.as_str(), tenant, AccountState::Online, None))
        .await
        .expect("account insert");
    id
}

#[tokio::test]
async fn holder_death_fences_the_old_holder_and_recovers_the_work() {
    let Some(store) = gated().await else {
        eprintln!("skip: {ENV} is not set (CI has no database)");
        return;
    };
    let tenant = unique_tenant("fault-death");
    store
        .upsert_tenant(&tenant_record(&tenant, "fault-death"))
        .await
        .expect("tenant insert");
    let w1 = register(&store, &tenant, "death-w1", "node-1", ts(9_000)).await;
    let w2 = register(&store, &tenant, "death-w2", "node-2", ts(9_100)).await;
    let scheduler = Scheduler::new(
        store.clone(),
        PlacementPolicy::new(4, 5_000).expect("valid"),
    );
    let acc = seed_account(&store, &tenant, "death-acc").await;
    let assigned = scheduler
        .assign(&tenant, &acc, &[], None, 60_000, ts(10_000))
        .await
        .expect("initial placement");
    assert_eq!(assigned.holder, w1);

    // The holder claims an operation and then dies: heartbeats stop
    // while the attempt stays open past its deadline.
    let op = OperationId::parse(unique_entity("op", "death")).expect("operation id");
    let attempt = AttemptId::parse(unique_entity("att", "death")).expect("attempt id");
    store
        .create_operation(&NewOperation {
            id: &op,
            tenant: &tenant,
            kind: "send_message",
            idempotency_key: &unique_entity("key", "death"),
            at: ts(10_000),
        })
        .await
        .expect("create operation");
    store
        .claim_operation(&tenant, &op, &w1, &attempt, ts(12_000), ts(10_000))
        .await
        .expect("the dead holder claims before dying");
    store
        .heartbeat_worker(&tenant, &w1, ts(10_000))
        .await
        .expect("last beat");
    store
        .heartbeat_worker(&tenant, &w2, ts(16_000))
        .await
        .expect("w2 keeps beating");

    // Takeover recovers the account onto the healthy worker with a
    // risen generation.
    let takeover = scheduler
        .take_over_lost(&tenant, 5_000, 60_000, ts(16_000))
        .await
        .expect("takeover");
    assert_eq!(takeover.migration.sources, vec![w1.clone()]);
    assert_eq!(takeover.migration.moved.len(), 1, "the account moved");
    let (moved_account, new_holder, generation) = &takeover.migration.moved[0];
    assert_eq!(moved_account, &acc);
    assert_eq!(new_holder, &w2);
    assert_eq!(*generation, 2, "the steal rises the fencing token");

    // The dead holder's renew is refused, naming the new holder and
    // generation — it learns it lost the account instead of being
    // shadowed.
    let fenced = store
        .renew_lease(&tenant, &acc, &w1, ts(100_000), ts(16_100))
        .await
        .expect_err("the old holder must be fenced");
    match &fenced {
        fishpond_storage::StorageError::Conflict { reason, .. } => {
            assert!(reason.contains(w2.as_str()), "{fenced}");
            assert!(reason.contains("generation 2"), "{fenced}");
        }
        other => panic!("expected a lease conflict, got {other:?}"),
    }

    // The abandoned attempt is reaped: it crossed its deadline, the
    // operation returns to PENDING, and a fresh claim bumps the
    // attempt number.
    let reaped = store
        .expired_attempts(&tenant, ts(16_200))
        .await
        .expect("reaper read");
    assert_eq!(reaped.len(), 1, "the abandoned attempt is due");
    assert_eq!(reaped[0].attempt.attempt_id, attempt);
    store
        .close_attempt(
            &tenant,
            &op,
            &attempt,
            &w1,
            AttemptClose::TimedOut,
            ts(16_200),
        )
        .await
        .expect("reaper closes the abandoned attempt");
    let pending = store
        .get_operation(&tenant, &op)
        .await
        .expect("operation stays");
    assert_eq!(
        pending.state,
        fishpond_domain::OperationState::Pending,
        "a timed-out attempt requeues the operation"
    );
    let retry = AttemptId::parse(unique_entity("att", "death-retry")).expect("attempt id");
    let reclaimed = store
        .claim_operation(&tenant, &op, &w2, &retry, ts(20_000), ts(16_300))
        .await
        .expect("the recovered holder reclaims");
    assert_eq!(
        reclaimed.attempt.attempt_no, 2,
        "the retry is a fresh, numbered attempt"
    );
    store
        .close_attempt(
            &tenant,
            &op,
            &retry,
            &w2,
            AttemptClose::Succeeded,
            ts(16_400),
        )
        .await
        .expect("the retry succeeds");
    let done = store
        .get_operation(&tenant, &op)
        .await
        .expect("operation stays");
    assert_eq!(done.state, fishpond_domain::OperationState::Succeeded);
}

#[tokio::test]
async fn a_drained_node_under_pressure_refuses_and_recovers() {
    let Some(store) = gated().await else {
        eprintln!("skip: {ENV} is not set (CI has no database)");
        return;
    };
    let tenant = unique_tenant("fault-drain");
    store
        .upsert_tenant(&tenant_record(&tenant, "fault-drain"))
        .await
        .expect("tenant insert");
    // node-1 carries two workers (capacity 4), node-2 one (capacity 2):
    // draining node-1 leaves room for exactly one of its four accounts.
    let w1 = register(&store, &tenant, "drainp-w1", "node-1", ts(9_000)).await;
    let w2 = register(&store, &tenant, "drainp-w2", "node-1", ts(9_100)).await;
    let w3 = register(&store, &tenant, "drainp-w3", "node-2", ts(9_200)).await;
    let scheduler = Scheduler::new(
        store.clone(),
        PlacementPolicy::new(2, 5_000).expect("valid"),
    );
    let mut accounts = Vec::new();
    for n in 0..5 {
        accounts.push(seed_account(&store, &tenant, &format!("drainp-{n}")).await);
    }
    // Seed the leases directly for a deterministic split: node-1's two
    // workers carry four accounts (capacity 2 each), node-2 carries one.
    for (i, acc) in accounts.iter().enumerate() {
        let holder = match i {
            0 | 1 => &w1,
            2 | 3 => &w2,
            _ => &w3,
        };
        store
            .acquire_lease(&tenant, acc, holder, ts(70_000), ts(10_000))
            .await
            .expect("seeded lease");
    }

    // Drain node-1: exactly one account fits on node-2; the rest are
    // unassigned — still there, never silently live on a drained node.
    let report = scheduler
        .drain_node(&tenant, "node-1", 60_000, ts(11_000))
        .await
        .expect("node drain");
    assert_eq!(report.migration.moved.len(), 1, "only one slot was free");
    assert_eq!(
        report.migration.moved[0].1, w3,
        "node-2 took the one account that fit"
    );
    assert_eq!(
        report.migration.unassigned.len(),
        3,
        "the rest had no eligible worker"
    );
    for (moved, _, _) in &report.migration.moved {
        let lease = store
            .get_lease(&tenant, moved)
            .await
            .expect("get")
            .expect("the moved account stays leased");
        assert_eq!(lease.holder, w3);
    }
    for acc in &accounts {
        if let Some(lease) = store.get_lease(&tenant, acc).await.expect("get") {
            assert!(
                lease.holder == w3 || lease.expires_at <= ts(11_000),
                "no live lease lingers on the drained node"
            );
        }
    }

    // A fresh placement under full pressure is refused, not dropped.
    let extra = seed_account(&store, &tenant, "drainp-extra").await;
    let refused = scheduler
        .assign(&tenant, &extra, &[], None, 60_000, ts(11_100))
        .await
        .expect_err("no capacity anywhere");
    assert!(
        matches!(refused, SchedulerError::NoEligibleWorker { .. }),
        "expected NoEligibleWorker, got {refused}"
    );

    // Undrain one member: its capacity returns and the pressure clears.
    store
        .undrain_worker(&tenant, &w1)
        .await
        .expect("undrain w1");
    let recovered = scheduler
        .assign(&tenant, &extra, &[], None, 60_000, ts(11_200))
        .await
        .expect("placement after undrain");
    assert_eq!(
        recovered.holder, w1,
        "the undrained worker takes work again"
    );
}
