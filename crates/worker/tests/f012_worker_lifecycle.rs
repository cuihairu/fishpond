//! F012 acceptance (worker crate): the daemon-facing services over the
//! storage CAS. T01 = registration + heartbeat, T02 = renewable lease
//! terms, T20 = steal semantics and generation fencing. (The T-numbers
//! come from the external plan document; the mapping follows the F012
//! TODO line's wording: 注册/心跳 → T01, lease/generation → T02, CAS →
//! T20.) Skipped when `FISHPOND_TEST_DATABASE_URL` is unset.

mod common;

use common::{ENV, account, gated, tenant_record, ts, unique_entity, unique_tenant};
use fishpond_domain::{AccountId, AccountState, TenantId, WorkerId};
use fishpond_storage::{StorageError, Store};
use fishpond_worker::{LeaseManager, Registration};

/// One tenant, one account, both services over the same store.
struct World {
    tenant: TenantId,
    account: AccountId,
    registration: Registration,
    leases: LeaseManager,
}

impl World {
    async fn new(tag: &str) -> Option<World> {
        let store: Store = gated().await?;
        let tenant = unique_tenant(tag);
        store
            .upsert_tenant(&tenant_record(&tenant, tag))
            .await
            .expect("tenant insert");
        let account_id = AccountId::parse(unique_entity("acc", tag)).expect("account id");
        store
            .upsert_account(&account(
                account_id.as_str(),
                &tenant,
                AccountState::Online,
                None,
            ))
            .await
            .expect("account insert");
        Some(World {
            tenant,
            account: account_id,
            registration: Registration::new(store.clone()),
            leases: LeaseManager::new(store),
        })
    }
}

fn worker_id(tag: &str, n: u32) -> WorkerId {
    WorkerId::parse(unique_entity("wrk", &format!("{tag}-{n}"))).expect("worker id")
}

#[tokio::test]
async fn t01_worker_registers_beats_and_is_detected_lost() {
    let Some(world) = World::new("f012-t01").await else {
        eprintln!("skip: {ENV} is not set (CI has no database)");
        return;
    };
    let worker = worker_id("f012-t01", 1);

    let registered = world
        .registration
        .register(
            &world.tenant,
            worker.clone(),
            "kode-1",
            &["browser"],
            ts(1_000),
        )
        .await
        .expect("register");
    assert_eq!(registered.capabilities, vec!["browser"]);

    world
        .registration
        .heartbeat(&world.tenant, &worker, ts(2_000))
        .await
        .expect("heartbeat");
    assert!(
        world
            .registration
            .lost(&world.tenant, ts(1_999))
            .await
            .expect("lost sweep")
            .is_empty()
    );
    let lost = world
        .registration
        .lost(&world.tenant, ts(2_000))
        .await
        .expect("lost sweep at cutoff");
    assert_eq!(lost.len(), 1);
    assert_eq!(lost[0].id, worker);

    // An unknown worker's heartbeat fails with NotFound: the daemon's
    // signal to re-register.
    let stranger = worker_id("f012-t01", 2);
    assert!(matches!(
        world
            .registration
            .heartbeat(&world.tenant, &stranger, ts(2_100))
            .await,
        Err(StorageError::NotFound { what: "worker", .. })
    ));

    assert!(
        world
            .registration
            .deregister(&world.tenant, &worker)
            .await
            .expect("deregister")
    );
}

#[tokio::test]
async fn t02_lease_terms_are_exclusive_and_renewable_by_the_holder() {
    let Some(world) = World::new("f012-t02").await else {
        eprintln!("skip: {ENV} is not set (CI has no database)");
        return;
    };
    let first = worker_id("f012-t02", 1);
    let second = worker_id("f012-t02", 2);

    let term = world
        .leases
        .acquire(&world.tenant, &world.account, &first, ts(3_000), ts(1_000))
        .await
        .expect("first worker takes the account");
    assert_eq!(term.generation, 1);

    let held = world
        .leases
        .acquire(&world.tenant, &world.account, &second, ts(3_000), ts(1_100))
        .await
        .expect_err("second worker must be refused");
    let StorageError::Conflict { reason, .. } = held else {
        panic!("expected Conflict, got {held}");
    };
    assert!(
        reason.contains("held by"),
        "conflict names the holder: {reason}"
    );

    let renewed = world
        .leases
        .renew(&world.tenant, &world.account, &first, ts(6_000), ts(2_000))
        .await
        .expect("holder renews");
    assert_eq!(renewed.generation, 1, "renewal never bumps the generation");
    assert_eq!(renewed.expires_at, ts(6_000));

    assert!(
        world
            .leases
            .release(&world.tenant, &world.account, &first)
            .await
            .expect("holder releases")
    );
    assert!(
        world
            .leases
            .get(&world.tenant, &world.account)
            .await
            .expect("get after release")
            .is_none()
    );
}

#[tokio::test]
async fn t20_expired_lease_is_stolen_with_a_new_generation() {
    let Some(world) = World::new("f012-t20").await else {
        eprintln!("skip: {ENV} is not set (CI has no database)");
        return;
    };
    let paused = worker_id("f012-t20", 1);
    let fresh = worker_id("f012-t20", 2);

    // The paused worker takes a short term and then goes silent.
    let original = world
        .leases
        .acquire(&world.tenant, &world.account, &paused, ts(2_000), ts(1_000))
        .await
        .expect("paused worker takes the account");
    assert_eq!(original.generation, 1);

    // Past expiry the fresh worker steals the account.
    let stolen = world
        .leases
        .acquire(&world.tenant, &world.account, &fresh, ts(8_000), ts(2_000))
        .await
        .expect("expired term is stealable");
    assert_eq!(stolen.holder, fresh);
    assert_eq!(stolen.generation, 2, "steal bumps the fencing token");

    // Fencing: the paused worker's keep-alive now fails and names the
    // new holder — its signal to stop acting on the account.
    let fencing = world
        .leases
        .renew(&world.tenant, &world.account, &paused, ts(8_100), ts(2_100))
        .await
        .expect_err("paused worker must not renew a stolen term");
    let StorageError::Conflict { reason, .. } = fencing else {
        panic!("expected Conflict, got {fencing}");
    };
    assert!(
        reason.contains(&fresh.as_str().to_owned()),
        "fencing error names the new holder: {reason}"
    );

    // The fresh worker hands the account back; a second release is a no-op.
    assert!(
        world
            .leases
            .release(&world.tenant, &world.account, &fresh)
            .await
            .expect("fresh worker releases")
    );
    assert!(
        !world
            .leases
            .release(&world.tenant, &world.account, &fresh)
            .await
            .expect("second release is a no-op")
    );
}
