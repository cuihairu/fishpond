//! F012 acceptance: worker registry — registration, heartbeat, liveness
//! detection and tenant isolation under row-level security.

mod common;

use common::{ENV, gated, tenant_record, ts, unique_entity, unique_tenant};
use fishpond_domain::{TenantId, WorkerId};
use fishpond_storage::{StorageError, Store};

async fn tenant_with_worker(store: &Store, tag: &str) -> (TenantId, WorkerId) {
    let tenant = unique_tenant(tag);
    store
        .upsert_tenant(&tenant_record(&tenant, tag))
        .await
        .expect("tenant insert");
    let id = WorkerId::parse(unique_entity("wrk", tag)).expect("worker id");
    (tenant, id)
}

#[tokio::test]
async fn registration_heartbeat_and_liveness_track_the_worker() {
    let Some(store) = gated().await else {
        eprintln!("skip: {ENV} is not set (CI has no database)");
        return;
    };
    let (tenant, id) = tenant_with_worker(&store, "reg").await;

    let registered = store
        .register_worker(&fishpond_storage::NewWorker {
            id: &id,
            tenant: &tenant,
            name: "kode-1",
            capabilities: &["browser", "xianyu"],
            at: ts(1_000),
        })
        .await
        .expect("register");
    assert_eq!(registered.id, id);
    assert_eq!(registered.capabilities, vec!["browser", "xianyu"]);
    assert_eq!(registered.registered_at, ts(1_000));
    assert_eq!(registered.last_heartbeat_at, ts(1_000));

    // Heartbeat advances only the liveness stamps.
    assert!(
        store
            .heartbeat_worker(&tenant, &id, ts(2_000))
            .await
            .expect("heartbeat of a registered worker succeeds")
    );
    let after = store.get_worker(&tenant, &id).await.expect("get");
    assert_eq!(after.last_heartbeat_at, ts(2_000));
    assert_eq!(after.registered_at, ts(1_000));

    // Lost detection is a read against the heartbeat cutoff.
    let lost_at = store
        .lost_workers(&tenant, ts(2_000))
        .await
        .expect("lost sweep");
    assert_eq!(lost_at.len(), 1, "heartbeat at the cutoff is lost");
    let alive_at = store
        .lost_workers(&tenant, ts(1_999))
        .await
        .expect("lost sweep");
    assert!(alive_at.is_empty(), "fresh heartbeat is not lost");

    // Re-registration refreshes identity but keeps the uptime record.
    let reregistered = store
        .register_worker(&fishpond_storage::NewWorker {
            id: &id,
            tenant: &tenant,
            name: "kode-1b",
            capabilities: &["browser"],
            at: ts(3_000),
        })
        .await
        .expect("re-register");
    assert_eq!(reregistered.name, "kode-1b");
    assert_eq!(reregistered.capabilities, vec!["browser"]);
    assert_eq!(reregistered.registered_at, ts(1_000));
    assert_eq!(reregistered.last_heartbeat_at, ts(3_000));

    // Listing sees exactly this worker.
    let all = store.list_workers(&tenant).await.expect("list");
    assert_eq!(all.len(), 1);
    assert_eq!(all[0].id, id);

    // Deregistration removes the row; the worker is then unknown.
    assert!(
        store
            .deregister_worker(&tenant, &id)
            .await
            .expect("deregister")
    );
    assert!(
        !store
            .deregister_worker(&tenant, &id)
            .await
            .expect("deregister again")
    );
    assert!(matches!(
        store.get_worker(&tenant, &id).await,
        Err(StorageError::NotFound { what: "worker", .. })
    ));
    assert!(
        !store
            .heartbeat_worker(&tenant, &id, ts(4_000))
            .await
            .expect("heartbeat of unknown worker")
    );
}

#[tokio::test]
async fn worker_rows_are_tenant_scoped() {
    let Some(store) = gated().await else {
        eprintln!("skip: {ENV} is not set (CI has no database)");
        return;
    };
    let (tenant_a, id) = tenant_with_worker(&store, "reg-a").await;
    let tenant_b = unique_tenant("reg-b");
    store
        .upsert_tenant(&tenant_record(&tenant_b, "reg-b"))
        .await
        .expect("tenant b insert");
    store
        .register_worker(&fishpond_storage::NewWorker {
            id: &id,
            tenant: &tenant_a,
            name: "kode-a",
            capabilities: &[],
            at: ts(1_000),
        })
        .await
        .expect("register in tenant a");

    // Tenant B cannot see, touch or heartbeat tenant A's worker: every
    // path answers "not found", never a cross-tenant leak.
    assert!(matches!(
        store.get_worker(&tenant_b, &id).await,
        Err(StorageError::NotFound { .. })
    ));
    assert!(
        store
            .list_workers(&tenant_b)
            .await
            .expect("list b")
            .is_empty()
    );
    assert!(
        store
            .lost_workers(&tenant_b, ts(9_999))
            .await
            .expect("lost b")
            .is_empty()
    );
    assert!(
        !store
            .heartbeat_worker(&tenant_b, &id, ts(2_000))
            .await
            .expect("heartbeat b")
    );
    assert!(
        !store
            .deregister_worker(&tenant_b, &id)
            .await
            .expect("deregister b")
    );

    // Unbound sessions (raw pool) see nothing: fail closed.
    let raw = store.pool();
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM workers")
        .fetch_one(raw)
        .await
        .expect("raw count");
    assert_eq!(count, 0, "RLS must hide every tenant row from raw reads");
}
