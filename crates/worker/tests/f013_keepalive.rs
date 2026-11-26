//! F013 acceptance: the worker keep-alive daemon — heartbeat, lease
//! renewal on an interval, and the one-shot steal-back policy
//! (re-take own expired terms, yield to a live foreign holder).

mod common;

use common::{ENV, account, gated, tenant_record, ts, unique_entity, unique_tenant};
use fishpond_domain::{AccountId, AccountState, TenantId, UnixMillis, WorkerId};
use fishpond_storage::{NewWorker, Store};
use fishpond_worker::{KeepAlive, KeepAliveConfig, RenewOutcome};

/// Registers one worker for `tenant` at `now`.
async fn register(store: &Store, tenant: &TenantId, tag: &str, now: UnixMillis) -> WorkerId {
    let id = WorkerId::parse(unique_entity("wrk", &format!("ka-{tag}"))).expect("worker id");
    store
        .register_worker(&NewWorker {
            id: &id,
            tenant,
            name: "keep-alive-worker",
            capabilities: &["browser"],
            at: now,
        })
        .await
        .expect("register");
    id
}

/// Seeds `n` accounts owned by `tenant`.
async fn accounts(store: &Store, tenant: &TenantId, tag: &str, n: u32) -> Vec<AccountId> {
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
async fn keepalive_renews_then_steals_back_own_expired_terms() {
    let Some(store) = gated().await else {
        eprintln!("skip: {ENV} is not set (CI has no database)");
        return;
    };
    let tenant = unique_tenant("ka-renew");
    store
        .upsert_tenant(&tenant_record(&tenant, "ka-renew"))
        .await
        .expect("tenant insert");
    let worker = register(&store, &tenant, "renew", ts(1_000)).await;
    let rival = register(&store, &tenant, "rival", ts(1_000)).await;
    let [a1, a2] = &accounts(&store, &tenant, "renew", 2).await[..] else {
        panic!("two accounts seeded")
    };
    let (a1, a2) = (a1.clone(), a2.clone());
    // Two leases on one worker, terms far from the tick clock.
    for account in [&a1, &a2] {
        store
            .acquire_lease(&tenant, account, &worker, ts(30_000), ts(1_000))
            .await
            .expect("initial acquire");
    }
    let keepalive = KeepAlive::new(
        store.clone(),
        tenant.clone(),
        worker.clone(),
        vec![a1.clone(), a2.clone()],
        KeepAliveConfig::new(10_000).expect("valid"),
    );

    // Tick inside the terms: both renew in place, generation unchanged.
    let report = keepalive.tick(ts(20_000)).await.expect("tick");
    assert!(report.heartbeat_ok);
    assert_eq!(report.accounts.len(), 2);
    for (account, outcome) in &report.accounts {
        let lease = store
            .get_lease(&tenant, account)
            .await
            .expect("get")
            .expect("leased");
        assert_eq!(
            outcome,
            &RenewOutcome::Renewed { generation: 1 },
            "renewal extends the term without a generation change"
        );
        assert_eq!(lease.expires_at, ts(30_000), "term moved to now + term");
    }

    // Sleep past the terms; while this worker is dark, a rival steals
    // a1 live. a2 goes unclaimed.
    store
        .acquire_lease(&tenant, &a1, &rival, ts(50_000), ts(40_000))
        .await
        .expect("rival steals a1 (w1 dark past expiry)");

    let report = keepalive.tick(ts(45_000)).await.expect("tick after expiry");
    assert!(report.heartbeat_ok);
    let outcome = |account: &AccountId| {
        report
            .accounts
            .iter()
            .find(|(a, _)| a == account)
            .map(|(_, o)| o.clone())
            .expect("every account reported")
    };
    // a1 is live on the rival: yield, naming the holder.
    assert_eq!(
        outcome(&a1),
        RenewOutcome::LostTo {
            holder: rival.clone()
        }
    );
    // a2 expired with nobody else on it: steal back our own term.
    match outcome(&a2) {
        RenewOutcome::Regained { generation } => {
            assert_eq!(
                generation, 1,
                "nobody else took it; generation does not move"
            );
        }
        other => panic!("expected Regained, got {other:?}"),
    }
    let a2_lease = store
        .get_lease(&tenant, &a2)
        .await
        .expect("get")
        .expect("a2 leased");
    assert_eq!(
        a2_lease.expires_at,
        ts(55_000),
        "fresh term from the steal-back"
    );
}

#[tokio::test]
async fn keepalive_takes_back_a_vanished_lease_row() {
    let Some(store) = gated().await else {
        eprintln!("skip: {ENV} is not set (CI has no database)");
        return;
    };
    let tenant = unique_tenant("ka-vanished");
    store
        .upsert_tenant(&tenant_record(&tenant, "ka-vanished"))
        .await
        .expect("tenant insert");
    let worker = register(&store, &tenant, "vanished", ts(1_000)).await;
    let [a1] = &accounts(&store, &tenant, "vanished", 1).await[..] else {
        panic!("one account seeded")
    };
    let a1 = a1.clone();
    store
        .acquire_lease(&tenant, &a1, &worker, ts(20_000), ts(1_000))
        .await
        .expect("acquire");
    // Another flow hands the account back: the lease row is gone.
    store
        .release_lease(&tenant, &a1, &worker)
        .await
        .expect("release");
    let keepalive = KeepAlive::new(
        store.clone(),
        tenant.clone(),
        worker.clone(),
        vec![a1.clone()],
        KeepAliveConfig::new(10_000).expect("valid"),
    );
    let report = keepalive.tick(ts(5_000)).await.expect("tick");
    assert_eq!(
        report.accounts[0].1,
        RenewOutcome::Regained { generation: 1 },
        "a vanished row is re-acquired freshly"
    );
    let lease = store
        .get_lease(&tenant, &a1)
        .await
        .expect("get")
        .expect("re-held");
    assert_eq!(
        lease.expires_at,
        ts(15_000),
        "term from the steal-back instant"
    );
}

#[tokio::test]
async fn keepalive_reports_a_missing_registration_but_keeps_renewing() {
    let Some(store) = gated().await else {
        eprintln!("skip: {ENV} is not set (CI has no database)");
        return;
    };
    let tenant = unique_tenant("ka-unregistered");
    store
        .upsert_tenant(&tenant_record(&tenant, "ka-unregistered"))
        .await
        .expect("tenant insert");
    let ghost = WorkerId::parse(unique_entity("wrk", "ka-ghost")).expect("worker id");
    let [a1] = &accounts(&store, &tenant, "ghost", 1).await[..] else {
        panic!("one account seeded")
    };
    let a1 = a1.clone();
    // The lease is soft-referenced to a worker row that does not exist.
    store
        .acquire_lease(&tenant, &a1, &ghost, ts(20_000), ts(1_000))
        .await
        .expect("acquire");
    let keepalive = KeepAlive::new(
        store.clone(),
        tenant.clone(),
        ghost.clone(),
        vec![a1.clone()],
        KeepAliveConfig::new(10_000).expect("valid"),
    );
    let report = keepalive.tick(ts(5_000)).await.expect("tick");
    assert!(
        !report.heartbeat_ok,
        "the caller is told the registration is gone"
    );
    assert_eq!(
        report.accounts[0].1,
        RenewOutcome::Renewed { generation: 1 },
        "renewal does not need the worker row (soft reference)"
    );
}

#[tokio::test]
async fn spawn_keeps_ticking_on_an_interval() {
    let Some(store) = gated().await else {
        eprintln!("skip: {ENV} is not set (CI has no database)");
        return;
    };
    let tenant = unique_tenant("ka-spawn");
    store
        .upsert_tenant(&tenant_record(&tenant, "ka-spawn"))
        .await
        .expect("tenant insert");
    let worker = register(&store, &tenant, "spawn", ts(1_000)).await;
    let [a1] = &accounts(&store, &tenant, "spawn", 1).await[..] else {
        panic!("one account seeded")
    };
    let a1 = a1.clone();
    store
        .acquire_lease(&tenant, &a1, &worker, ts(1_700_000_000_000), ts(1_000))
        .await
        .expect("acquire");
    let keepalive = KeepAlive::new(
        store.clone(),
        tenant.clone(),
        worker.clone(),
        vec![a1.clone()],
        KeepAliveConfig::new(600_000).expect("valid"),
    );
    let handle = keepalive.spawn(std::time::Duration::from_millis(20));
    // Real clock: poll until the loop's first tick (interval tickers
    // fire immediately) has landed a heartbeat past the registration
    // instant, then stop the daemon.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let row = store.get_worker(&tenant, &worker).await.expect("get");
        if row.last_heartbeat_at.as_millis() > ts(1_000).as_millis() {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the loop never heartbeated"
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    handle.abort();
    // The aborted daemon leaves the lease exactly as it left it: held
    // and unexpired.
    let lease = store
        .get_lease(&tenant, &a1)
        .await
        .expect("get")
        .expect("held");
    assert_eq!(lease.holder, worker);
}
