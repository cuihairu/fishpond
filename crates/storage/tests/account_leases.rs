//! F012 acceptance: account leases — CAS acquire/renew/release,
//! generation fencing on steal, tenant isolation.

mod common;

use common::{ENV, account, gated, tenant_record, ts, unique_entity, unique_tenant};
use fishpond_domain::{AccountId, AccountState, TenantId, WorkerId};
use fishpond_storage::{StorageError, Store};

/// One tenant with a registered account; returns tenant and account ids.
async fn tenant_with_account(store: &Store, tag: &str) -> (TenantId, AccountId) {
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
    (tenant, account_id)
}

fn holder(tag: &str, n: u32) -> WorkerId {
    WorkerId::parse(unique_entity("wrk", &format!("{tag}-{n}"))).expect("worker id")
}

#[tokio::test]
async fn lease_cas_lifecycle_and_generation_fencing() {
    let Some(store) = gated().await else {
        eprintln!("skip: {ENV} is not set (CI has no database)");
        return;
    };
    let (tenant, account) = tenant_with_account(&store, "lease").await;
    let w1 = holder("lease", 1);
    let w2 = holder("lease", 2);

    // Fresh acquire: generation 1.
    let first = store
        .acquire_lease(&tenant, &account, &w1, ts(2_000), ts(1_000))
        .await
        .expect("fresh acquire");
    assert_eq!(first.holder, w1);
    assert_eq!(first.generation, 1);
    assert_eq!(first.expires_at, ts(2_000));

    // A live foreign acquire loses the CAS with the holder named.
    let conflict = store
        .acquire_lease(&tenant, &account, &w2, ts(2_100), ts(1_100))
        .await
        .expect_err("live foreign acquire must conflict");
    let StorageError::Conflict { reason, .. } = conflict else {
        panic!("expected Conflict, got {conflict}");
    };
    assert!(
        reason.contains(&w1.as_str().to_owned()),
        "names holder: {reason}"
    );
    assert!(
        reason.contains("generation 1"),
        "names generation: {reason}"
    );

    // Renewal extends the holder's own term; generation stays 1.
    let renewed = store
        .renew_lease(&tenant, &account, &w1, ts(5_000), ts(1_500))
        .await
        .expect("renew by holder");
    assert_eq!(renewed.generation, 1);
    assert_eq!(renewed.expires_at, ts(5_000));
    assert_eq!(renewed.acquired_at, ts(1_000), "term start is kept");

    // A foreign renew is refused.
    assert!(matches!(
        store.renew_lease(&tenant, &account, &w2, ts(5_100), ts(1_600)).await,
        Err(StorageError::Conflict { reason, .. }) if reason.contains(&w1.as_str().to_owned())
    ));

    // A foreign release is a no-op; the holder's release removes the row.
    assert!(
        !store
            .release_lease(&tenant, &account, &w2)
            .await
            .expect("foreign release")
    );
    assert!(
        store
            .release_lease(&tenant, &account, &w1)
            .await
            .expect("holder release")
    );
    assert!(
        store
            .get_lease(&tenant, &account)
            .await
            .expect("get")
            .is_none()
    );

    // Steal after expiry: the old term is taken over, generation bumps.
    let w1_term = store
        .acquire_lease(&tenant, &account, &w1, ts(6_000), ts(5_500))
        .await
        .expect("w1 re-acquire");
    assert_eq!(w1_term.generation, 1, "fresh row starts at 1 again");
    let stolen = store
        .acquire_lease(&tenant, &account, &w2, ts(9_000), ts(6_000))
        .await
        .expect("steal after expiry");
    assert_eq!(stolen.holder, w2);
    assert_eq!(stolen.generation, 2, "steal increments the fencing token");
    assert_eq!(stolen.acquired_at, ts(6_000));

    // Fencing: the old holder's renew fails and names the new holder.
    let fencing = store
        .renew_lease(&tenant, &account, &w1, ts(9_500), ts(6_100))
        .await
        .expect_err("stolen holder must not renew");
    let StorageError::Conflict { reason, .. } = fencing else {
        panic!("expected Conflict, got {fencing}");
    };
    assert!(
        reason.contains(&w2.as_str().to_owned()),
        "names thief: {reason}"
    );

    // Renewal after *own* expiry is refused too: expired terms are never
    // silently extended; the worker must re-acquire.
    store
        .release_lease(&tenant, &account, &w2)
        .await
        .expect("cleanup");
    store
        .acquire_lease(&tenant, &account, &w1, ts(7_000), ts(6_500))
        .await
        .expect("w1 short term");
    let expired_renew = store
        .renew_lease(&tenant, &account, &w1, ts(9_000), ts(7_000))
        .await
        .expect_err("expired term must not renew in place");
    let StorageError::Conflict { reason, .. } = expired_renew else {
        panic!("expected Conflict, got {expired_renew}");
    };
    assert!(
        reason.contains("acquire again"),
        "points at re-acquire: {reason}"
    );
}

#[tokio::test]
async fn leases_are_tenant_scoped() {
    let Some(store) = gated().await else {
        eprintln!("skip: {ENV} is not set (CI has no database)");
        return;
    };
    let (tenant_a, account) = tenant_with_account(&store, "lease-a").await;
    let tenant_b = unique_tenant("lease-b");
    store
        .upsert_tenant(&tenant_record(&tenant_b, "lease-b"))
        .await
        .expect("tenant b insert");
    let holder_a = holder("lease-a", 3);
    let holder_b = holder("lease-b", 4);

    store
        .acquire_lease(&tenant_a, &account, &holder_a, ts(5_000), ts(1_000))
        .await
        .expect("tenant a acquire");

    // Tenant B sees no lease at all and cannot release tenant A's.
    assert!(
        store
            .get_lease(&tenant_b, &account)
            .await
            .expect("get b")
            .is_none()
    );
    assert!(
        !store
            .release_lease(&tenant_b, &account, &holder_b)
            .await
            .expect("release b")
    );

    // Unbound sessions (raw pool) see nothing: fail closed.
    let raw = store.pool();
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM account_leases")
        .fetch_one(raw)
        .await
        .expect("raw count");
    assert_eq!(count, 0, "RLS must hide every tenant row from raw reads");

    // Tenant A still holds its lease.
    let lease = store
        .get_lease(&tenant_a, &account)
        .await
        .expect("get a")
        .expect("lease survives the isolation probes");
    assert_eq!(lease.holder, holder_a);
    assert_eq!(lease.generation, 1);
}
