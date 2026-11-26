//! F037 acceptance (node drain): a deployment node registers several
//! workers under one host name, and `drain_node` is the operator face
//! for "take that whole machine down": every worker of the tenant
//! registered under the name drains (idempotently, per worker), its
//! live leases migrate to healthy workers elsewhere, and placement
//! never returns to the node. The lookup is tenant-scoped — the same
//! host name in another tenant is invisible, and an unknown name is a
//! refused operation, not a silent success over nothing.

mod common;

use common::{ENV, account, gated, tenant_record, ts, unique_entity, unique_tenant};
use fishpond_domain::{AccountId, AccountState, TenantId, WorkerId};
use fishpond_scheduler::{PlacementPolicy, Scheduler, SchedulerError};
use fishpond_storage::{NewWorker, Store};

/// Registers one worker with an explicit node name; returns its id.
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
async fn drain_node_drains_every_member_and_never_returns_there() {
    let Some(store) = gated().await else {
        eprintln!("skip: {ENV} is not set (CI has no database)");
        return;
    };
    let tenant = unique_tenant("node-drain");
    store
        .upsert_tenant(&tenant_record(&tenant, "node-drain"))
        .await
        .expect("tenant insert");
    // Node "node-a" is a two-worker host; "node-b" is the survivor.
    let a1 = register(&store, &tenant, "drain-a1", "node-a", ts(9_000)).await;
    let a2 = register(&store, &tenant, "drain-a2", "node-a", ts(9_100)).await;
    let b1 = register(&store, &tenant, "drain-b1", "node-b", ts(9_200)).await;
    // Capacity 4: node-b absorbs everything node-a holds.
    let scheduler = Scheduler::new(
        store.clone(),
        PlacementPolicy::new(4, 5_000).expect("valid"),
    );
    let a_1 = seed_account(&store, &tenant, "drain-one").await;
    let a_2 = seed_account(&store, &tenant, "drain-two").await;
    let a_3 = seed_account(&store, &tenant, "drain-three").await;
    // Both accounts land on node-a (first by age tie-break, second by
    // stickiness).
    scheduler
        .assign(&tenant, &a_1, &[], None, 60_000, ts(10_000))
        .await
        .expect("placement");
    scheduler
        .assign(&tenant, &a_2, &[], Some(&a1), 60_000, ts(10_000))
        .await
        .expect("pinned placement");
    assert_eq!(
        store
            .get_lease(&tenant, &a_1)
            .await
            .expect("get")
            .expect("leased")
            .holder,
        a1,
        "the first account sits on node-a"
    );
    assert_eq!(
        store
            .get_lease(&tenant, &a_2)
            .await
            .expect("get")
            .expect("leased")
            .holder,
        a1,
        "the pinned account sits on node-a"
    );

    // Drain the node: both members drain, both accounts migrate to
    // node-b with a risen generation.
    let report = scheduler
        .drain_node(&tenant, "node-a", 60_000, ts(11_000))
        .await
        .expect("node drain");
    assert_eq!(report.node, "node-a");
    let mut members = report.workers.clone();
    members.sort();
    let mut expected = vec![a1.clone(), a2.clone()];
    expected.sort();
    assert_eq!(members, expected, "both node-a workers drained");
    assert_eq!(
        report.already_draining, 0,
        "fresh drain, nobody pre-drained"
    );
    assert_eq!(report.migration.moved.len(), 2, "both accounts migrated");
    assert!(report.migration.contested.is_empty());
    assert!(report.migration.unassigned.is_empty());
    for (account, holder, generation) in &report.migration.moved {
        assert_eq!(holder, &b1, "node-b is the only healthy target");
        assert_eq!(*generation, 2, "migration is a steal: generation rises");
        let lease = store
            .get_lease(&tenant, account)
            .await
            .expect("get")
            .expect("leased");
        assert_eq!(&lease.holder, holder);
        assert_eq!(lease.expires_at, ts(71_000));
    }
    let drained_a2 = store.get_worker(&tenant, &a2).await.expect("worker");
    assert!(drained_a2.draining, "the second member is marked draining");
    let survivor = store.get_worker(&tenant, &b1).await.expect("worker");
    assert!(!survivor.draining, "node-b stays healthy");

    // Idempotent second pass: everyone already draining, nothing moves.
    let again = scheduler
        .drain_node(&tenant, "node-a", 60_000, ts(11_100))
        .await
        .expect("second drain");
    assert_eq!(again.already_draining, 2);
    assert!(again.migration.moved.is_empty());

    // Placement never returns to the drained node.
    let fresh = scheduler
        .assign(&tenant, &a_3, &[], None, 60_000, ts(11_200))
        .await
        .expect("placement after drain");
    assert_eq!(fresh.holder, b1, "the drained node is skipped");
}

#[tokio::test]
async fn drain_node_is_tenant_scoped() {
    let Some(store) = gated().await else {
        eprintln!("skip: {ENV} is not set (CI has no database)");
        return;
    };
    // Two tenants, each with a worker registered under the SAME host
    // name and a spare to receive migrations. Worker ids are globally
    // unique; only the name collides.
    let ta = unique_tenant("node-ta");
    let tb = unique_tenant("node-tb");
    for (tenant, name) in [(&ta, "node-ta"), (&tb, "node-tb")] {
        store
            .upsert_tenant(&tenant_record(tenant, name))
            .await
            .expect("tenant insert");
    }
    let ta_node = register(&store, &ta, "ta-node", "shared-node", ts(9_000)).await;
    let ta_spare = register(&store, &ta, "ta-spare", "ta-other", ts(9_100)).await;
    let tb_node = register(&store, &tb, "tb-node", "shared-node", ts(9_200)).await;
    let ta_acc = seed_account(&store, &ta, "ta-acc").await;
    let tb_acc = seed_account(&store, &tb, "tb-acc").await;
    let scheduler = Scheduler::new(
        store.clone(),
        PlacementPolicy::new(4, 5_000).expect("valid"),
    );
    scheduler
        .assign(&ta, &ta_acc, &[], None, 60_000, ts(10_000))
        .await
        .expect("tenant a placement");
    scheduler
        .assign(&tb, &tb_acc, &[], None, 60_000, ts(10_000))
        .await
        .expect("tenant b placement");

    // Draining "shared-node" for tenant a only sees tenant a's member.
    let report = scheduler
        .drain_node(&ta, "shared-node", 60_000, ts(11_000))
        .await
        .expect("node drain");
    assert_eq!(
        report.workers,
        vec![ta_node.clone()],
        "only the own-tenant member"
    );
    assert_eq!(report.migration.moved.len(), 1);
    assert_eq!(
        report.migration.moved[0].1, ta_spare,
        "the migration stays inside tenant a"
    );

    // Tenant b's same-named worker is untouched: still draining=false,
    // still holding its lease.
    let untouched = store.get_worker(&tb, &tb_node).await.expect("worker");
    assert!(!untouched.draining, "the other tenant's node never drained");
    let tb_lease = store
        .get_lease(&tb, &tb_acc)
        .await
        .expect("get")
        .expect("tenant b keeps its live lease");
    assert_eq!(tb_lease.holder, tb_node);
    assert!(tb_lease.expires_at > ts(11_000), "the term was never cut");
}

#[tokio::test]
async fn drain_node_refuses_unknown_and_foreign_names() {
    let Some(store) = gated().await else {
        eprintln!("skip: {ENV} is not set (CI has no database)");
        return;
    };
    let ta = unique_tenant("node-unknown-a");
    let tb = unique_tenant("node-unknown-b");
    for (tenant, name) in [(&ta, "node-ua"), (&tb, "node-ub")] {
        store
            .upsert_tenant(&tenant_record(tenant, name))
            .await
            .expect("tenant insert");
    }
    register(&store, &tb, "ub-node", "node-ub", ts(9_000)).await;
    let scheduler = Scheduler::new(
        store.clone(),
        PlacementPolicy::new(4, 5_000).expect("valid"),
    );

    // A name nobody registered is refused, not a silent success.
    let unknown = scheduler
        .drain_node(&ta, "no-such-node", 60_000, ts(10_000))
        .await
        .expect_err("unknown node");
    assert!(
        matches!(&unknown, SchedulerError::UnknownNode { node, .. } if node == "no-such-node"),
        "expected UnknownNode, got {unknown}"
    );

    // A name that exists only in ANOTHER tenant is equally unknown
    // here — the lookup never crosses the tenant boundary.
    let foreign = scheduler
        .drain_node(&ta, "node-ub", 60_000, ts(10_000))
        .await
        .expect_err("foreign node name");
    assert!(
        matches!(foreign, SchedulerError::UnknownNode { .. }),
        "expected UnknownNode for the other tenant's node, got {foreign}"
    );
}
