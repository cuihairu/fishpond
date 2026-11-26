//! F013 acceptance: worker drain state and lease takeover primitives.
//!
//! Drain is persisted control-plane state (idempotent request, frozen
//! first-request stamp, sticky across re-registration). Force-expiry is
//! the takeover action: it cuts live terms without touching
//! `generation`, so the next acquire takes the steal path and the
//! fencing token keeps rising. Both surfaces are tenant-scoped under
//! the same row-level security as the rest of the schema.

mod common;

use common::{ENV, account, gated, tenant_record, ts, unique_entity, unique_tenant};
use fishpond_domain::{AccountId, AccountState, TenantId, WorkerId};
use fishpond_storage::Store;
use sqlx::Executor;
use sqlx::Row;

/// One tenant with `n` registered accounts; returns tenant and accounts.
async fn tenant_with_accounts(store: &Store, tag: &str, n: u32) -> (TenantId, Vec<AccountId>) {
    let tenant = unique_tenant(tag);
    store
        .upsert_tenant(&tenant_record(&tenant, tag))
        .await
        .expect("tenant insert");
    let mut accounts = Vec::new();
    for i in 1..=n {
        let id = AccountId::parse(unique_entity("acc", &format!("{tag}-{i}"))).expect("account id");
        store
            .upsert_account(&account(id.as_str(), &tenant, AccountState::Online, None))
            .await
            .expect("account insert");
        accounts.push(id);
    }
    (tenant, accounts)
}

fn worker_id(tag: &str, n: u32) -> WorkerId {
    WorkerId::parse(unique_entity("wrk", &format!("{tag}-{n}"))).expect("worker id")
}

#[tokio::test]
async fn drain_flag_is_idempotent_and_survives_reregistration() {
    let Some(store) = gated().await else {
        eprintln!("skip: {ENV} is not set (CI has no database)");
        return;
    };
    let tenant = unique_tenant("drain-flag");
    store
        .upsert_tenant(&tenant_record(&tenant, "drain-flag"))
        .await
        .expect("tenant insert");
    let worker = worker_id("drain-flag", 1);
    store
        .register_worker(&fishpond_storage::NewWorker {
            id: &worker,
            tenant: &tenant,
            name: "node-a",
            capabilities: &["browser"],
            at: ts(1_000),
        })
        .await
        .expect("register");

    // Before any drain: flag down, no stamp.
    let before = store.get_worker(&tenant, &worker).await.expect("worker");
    assert!(!before.draining);
    assert_eq!(before.drain_requested_at, None);

    // First drain request stamps `drain_requested_at`.
    let drained = store
        .drain_worker(&tenant, &worker, ts(5_000))
        .await
        .expect("drain")
        .expect("worker exists");
    assert!(drained.draining);
    assert_eq!(drained.drain_requested_at, Some(ts(5_000)));

    // A second request is idempotent: the first stamp is frozen.
    let again = store
        .drain_worker(&tenant, &worker, ts(6_000))
        .await
        .expect("re-drain")
        .expect("worker exists");
    assert_eq!(again.drain_requested_at, Some(ts(5_000)));

    // Re-registration refreshes the heartbeat but must not clear the
    // in-flight drain.
    let reregistered = store
        .register_worker(&fishpond_storage::NewWorker {
            id: &worker,
            tenant: &tenant,
            name: "node-a",
            capabilities: &["browser", "xianyu"],
            at: ts(7_000),
        })
        .await
        .expect("re-register");
    assert!(reregistered.draining);
    assert_eq!(reregistered.drain_requested_at, Some(ts(5_000)));
    assert_eq!(reregistered.last_heartbeat_at, ts(7_000));

    // Un-drain clears both the flag and the stamp; the next drain is a
    // fresh request.
    let cleared = store
        .undrain_worker(&tenant, &worker)
        .await
        .expect("undrain")
        .expect("worker exists");
    assert!(!cleared.draining);
    assert_eq!(cleared.drain_requested_at, None);

    // Unknown workers report `None` on both surfaces.
    let ghost = worker_id("drain-flag", 99);
    assert!(
        store
            .drain_worker(&tenant, &ghost, ts(8_000))
            .await
            .expect("drain ghost")
            .is_none()
    );
    assert!(
        store
            .undrain_worker(&tenant, &ghost)
            .await
            .expect("undrain ghost")
            .is_none()
    );
}

#[tokio::test]
async fn force_expire_cuts_only_the_holders_live_terms_and_keeps_generation() {
    let Some(store) = gated().await else {
        eprintln!("skip: {ENV} is not set (CI has no database)");
        return;
    };
    let (tenant, accounts) = tenant_with_accounts(&store, "force-expire", 3).await;
    let [a1, a2, a3] = &accounts[..] else {
        panic!("three accounts seeded")
    };
    let a1 = a1.clone();
    let a2 = a2.clone();
    let a3 = a3.clone();
    let w1 = worker_id("force-expire", 1);
    let w2 = worker_id("force-expire", 2);
    let w3 = worker_id("force-expire", 3);

    // w1 holds a1 (expires 2_000) and a2 (expires 3_000); w2 holds a3.
    store
        .acquire_lease(&tenant, &a1, &w1, ts(2_000), ts(1_000))
        .await
        .expect("w1 -> a1");
    store
        .acquire_lease(&tenant, &a2, &w1, ts(3_000), ts(1_100))
        .await
        .expect("w1 -> a2");
    store
        .acquire_lease(&tenant, &a3, &w2, ts(3_000), ts(1_200))
        .await
        .expect("w2 -> a3");

    // Live listing at 1_500: all three, oldest acquisition first.
    let live = store
        .list_live_leases(&tenant, ts(1_500))
        .await
        .expect("live list");
    assert_eq!(
        live.iter()
            .map(|l| l.account_id.clone())
            .collect::<Vec<_>>(),
        vec![a1.clone(), a2.clone(), a3.clone()],
        "ordered by acquired_at"
    );

    // At 2_000 the a1 term has expired (expires_at <= at) and drops out.
    let live = store
        .list_live_leases(&tenant, ts(2_000))
        .await
        .expect("live list");
    assert_eq!(
        live.iter()
            .map(|l| l.account_id.clone())
            .collect::<Vec<_>>(),
        vec![a2.clone(), a3.clone()]
    );

    // Force-expiry cuts exactly w1's live terms: a2 (a1 is already
    // expired), leaves w2 alone, and never touches `generation`.
    let cut = store
        .force_expire_leases(&tenant, &w1, ts(2_000))
        .await
        .expect("force expire");
    assert_eq!(cut, 1);
    let a2_lease = store
        .get_lease(&tenant, &a2)
        .await
        .expect("get")
        .expect("a2 leased");
    assert_eq!(a2_lease.expires_at, ts(2_000));
    assert_eq!(a2_lease.generation, 1);
    let a3_lease = store
        .get_lease(&tenant, &a3)
        .await
        .expect("get")
        .expect("a3 leased");
    assert_eq!(a3_lease.expires_at, ts(3_000), "w2 untouched");

    // The cut term is stealable: w3's acquire takes the steal path and
    // the fencing token rises to 2.
    let stolen = store
        .acquire_lease(&tenant, &a2, &w3, ts(4_000), ts(2_100))
        .await
        .expect("steal after force-expiry");
    assert_eq!(stolen.holder, w3);
    assert_eq!(stolen.generation, 2);

    // Idempotent: a second force-expiry cuts nothing.
    let cut_again = store
        .force_expire_leases(&tenant, &w1, ts(2_000))
        .await
        .expect("force expire again");
    assert_eq!(cut_again, 0);
}

#[tokio::test]
async fn drain_and_takeover_surfaces_are_tenant_scoped() {
    let Some(store) = gated().await else {
        eprintln!("skip: {ENV} is not set (CI has no database)");
        return;
    };
    let (tenant_a, accounts) = tenant_with_accounts(&store, "scoping-a", 1).await;
    let account_a = accounts.into_iter().next().expect("one account");
    let worker_a = worker_id("scoping-a", 1);
    store
        .register_worker(&fishpond_storage::NewWorker {
            id: &worker_a,
            tenant: &tenant_a,
            name: "node-a",
            capabilities: &["browser"],
            at: ts(1_000),
        })
        .await
        .expect("register a");
    store
        .acquire_lease(&tenant_a, &account_a, &worker_a, ts(9_000), ts(1_000))
        .await
        .expect("lease a");

    let tenant_b = unique_tenant("scoping-b");
    store
        .upsert_tenant(&tenant_record(&tenant_b, "scoping-b"))
        .await
        .expect("tenant b insert");

    // Tenant B cannot drain tenant A's worker.
    let foreign = store
        .drain_worker(&tenant_b, &worker_a, ts(2_000))
        .await
        .expect("foreign drain");
    assert!(foreign.is_none(), "RLS hides A's worker from B");

    // Tenant B cannot cut tenant A's live terms.
    let cut = store
        .force_expire_leases(&tenant_b, &worker_a, ts(2_000))
        .await
        .expect("foreign force-expire");
    assert_eq!(cut, 0);
    let lease = store
        .get_lease(&tenant_a, &account_a)
        .await
        .expect("get")
        .expect("A's lease intact");
    assert_eq!(lease.expires_at, ts(9_000), "term untouched by B");

    // Tenant B sees no live leases; an unbound pool sees no rows at all.
    assert!(
        store
            .list_live_leases(&tenant_b, ts(2_000))
            .await
            .expect("B live list")
            .is_empty()
    );
    let raw_workers: i64 = store
        .pool()
        .acquire()
        .await
        .expect("raw pool")
        .fetch_one("SELECT count(*) AS n FROM workers")
        .await
        .map(|row| row.get::<i64, _>("n"))
        .expect("unbound workers count");
    assert_eq!(raw_workers, 0, "unbound sessions read zero worker rows");
    let raw_leases: i64 = store
        .pool()
        .acquire()
        .await
        .expect("raw pool")
        .fetch_one("SELECT count(*) AS n FROM account_leases")
        .await
        .map(|row| row.get::<i64, _>("n"))
        .expect("unbound leases count");
    assert_eq!(raw_leases, 0, "unbound sessions read zero lease rows");
}
