//! F039 acceptance (capacity report): one tenant-bound query counts
//! the tenant's demand-vs-supply picture — accounts by state, workers
//! and their draining flag, leases live at the report instant,
//! operations by state, event log volume — and nothing from any other
//! tenant (row-level security plus the explicit `WHERE tenant_id`
//! agree even on a cross-table aggregate). Leases past their expiry
//! stop counting as live. The rendered report is secret-free.
//! Database-gated like every DB-backed suite.

use fishpond_cli::report::{TenantReport, render, run_report};
use fishpond_domain::{
    Account, AccountId, AccountState, AttemptId, EventId, OperationId, TenantId, UnixMillis,
    WorkerId,
};
use fishpond_storage::{
    AttemptClose, CapacityReport, NewEvent, NewOperation, NewWorker, Store, TenantRecord,
};

const TEST_DB_ENV: &str = "FISHPOND_TEST_DATABASE_URL";

fn test_db_url() -> Option<String> {
    std::env::var(TEST_DB_ENV)
        .ok()
        .filter(|v| !v.trim().is_empty())
}

fn millis(value: i64) -> UnixMillis {
    UnixMillis::from_millis(value).expect("timestamp")
}

/// Nanoseconds for per-run uniqueness (global primary keys, no cleanup).
fn unique_suffix() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_nanos()
}

fn unique_tenant(tag: &str) -> TenantId {
    TenantId::parse(format!("f039-rep-{tag}-{}", unique_suffix())).expect("tenant id")
}

fn account(id: &str, tenant: &TenantId, state: AccountState) -> Account {
    Account::new(
        AccountId::parse(id).expect("account id"),
        tenant.clone(),
        "mock",
        state,
        None,
        millis(2_000),
    )
    .expect("account satisfies its invariants")
}

async fn seed_tenant(store: &Store, tenant: &TenantId, name: &str) {
    store
        .upsert_tenant(&TenantRecord {
            id: tenant.clone(),
            name: name.to_owned(),
            created_at: millis(1_000),
        })
        .await
        .expect("tenant insert");
}

async fn register_worker(store: &Store, tenant: &TenantId, tag: &str, node: &str) -> WorkerId {
    let id = WorkerId::parse(format!("wrk-rep-{tag}-{}", unique_suffix())).expect("worker id");
    store
        .register_worker(&NewWorker {
            id: &id,
            tenant,
            name: node,
            capabilities: &["browser"],
            at: millis(9_000),
        })
        .await
        .expect("worker insert");
    id
}

async fn seed_account(
    store: &Store,
    tenant: &TenantId,
    tag: &str,
    state: AccountState,
) -> AccountId {
    let id = AccountId::parse(format!("acc-rep-{tag}-{}", unique_suffix())).expect("account id");
    store
        .upsert_account(&account(id.as_str(), tenant, state))
        .await
        .expect("account insert");
    id
}

async fn seed_event(store: &Store, tenant: &TenantId, acc: &AccountId, tag: &str) {
    let id = EventId::parse(format!("evt-rep-{tag}-{}", unique_suffix())).expect("event id");
    let payload = fishpond_domain::EventPayload::parse(r#"{"kind":"tick"}"#).expect("payload");
    store
        .append_events(
            tenant,
            &[NewEvent {
                id: &id,
                event_type: "report.tick",
                account_id: Some(acc),
                occurred_at: millis(3_000),
                payload: &payload,
            }],
        )
        .await
        .expect("event append");
}

#[tokio::test]
async fn report_counts_demand_and_supply_of_one_tenant() {
    let Some(url) = test_db_url() else {
        eprintln!("skip: no database");
        return;
    };
    let store = Store::connect(&url).await.expect("connect + migrate");
    let tenant = unique_tenant("full");
    seed_tenant(&store, &tenant, "f039-rep-full").await;

    // Demand side: an online and an offline account; one live lease and
    // one already-expired lease (acc2's expired before the instant).
    let acc1 = seed_account(&store, &tenant, "acc1", AccountState::Online).await;
    let acc2 = seed_account(&store, &tenant, "acc2", AccountState::Offline).await;
    let w1 = register_worker(&store, &tenant, "w1", "node-1").await;
    let w2 = register_worker(&store, &tenant, "w2", "node-1").await;
    store
        .drain_worker(&tenant, &w1, millis(9_500))
        .await
        .expect("drain w1");
    store
        .acquire_lease(&tenant, &acc1, &w1, millis(100_000), millis(1_000))
        .await
        .expect("live lease");
    store
        .acquire_lease(&tenant, &acc2, &w2, millis(2_500), millis(1_000))
        .await
        .expect("soon-expired lease");

    // Operations in all three buckets: pending, running, terminal.
    let mut ops = Vec::new();
    for n in 0..3 {
        let op = OperationId::parse(format!("op-rep-{n}-{}", unique_suffix())).expect("op id");
        store
            .create_operation(&NewOperation {
                id: &op,
                tenant: &tenant,
                kind: "send_message",
                idempotency_key: &format!("key-rep-{n}-{}", unique_suffix()),
                at: millis(10_000),
            })
            .await
            .expect("operation insert");
        ops.push(op);
    }
    let attempt = AttemptId::parse(format!("att-rep-{}", unique_suffix())).expect("attempt id");
    store
        .claim_operation(
            &tenant,
            &ops[1],
            &w1,
            &attempt,
            millis(60_000),
            millis(2_000),
        )
        .await
        .expect("claim makes op2 running");
    let attempt2 = AttemptId::parse(format!("att-rep-{}", unique_suffix())).expect("attempt id");
    store
        .claim_operation(
            &tenant,
            &ops[2],
            &w1,
            &attempt2,
            millis(60_000),
            millis(2_100),
        )
        .await
        .expect("claim makes op3 running");
    store
        .close_attempt(
            &tenant,
            &ops[2],
            &attempt2,
            &w1,
            AttemptClose::Succeeded,
            millis(2_200),
        )
        .await
        .expect("close makes op3 terminal");

    // The audit log carries two events.
    seed_event(&store, &tenant, &acc1, "e1").await;
    seed_event(&store, &tenant, &acc1, "e2").await;

    // Profiles, sessions and webhook subscriptions stay empty here —
    // the query still proves it reads those tables without tripping.
    let report = store
        .tenant_capacity(&tenant, millis(3_000))
        .await
        .expect("capacity report");
    assert_eq!(
        report,
        CapacityReport {
            accounts_total: 2,
            accounts_online: 1,
            workers_total: 2,
            workers_draining: 1,
            live_leases: 1,
            operations_pending: 1,
            operations_running: 1,
            operations_terminal: 1,
            events_total: 2,
            profiles_active: 0,
            sessions_active: 0,
            webhook_subscriptions: 0,
        }
    );
}

#[tokio::test]
async fn report_never_leaks_another_tenants_rows() {
    let Some(url) = test_db_url() else {
        eprintln!("skip: no database");
        return;
    };
    let store = Store::connect(&url).await.expect("connect + migrate");
    let ta = unique_tenant("iso-a");
    let tb = unique_tenant("iso-b");
    seed_tenant(&store, &ta, "f039-rep-iso-a").await;
    seed_tenant(&store, &tb, "f039-rep-iso-b").await;

    let acc_a = seed_account(&store, &ta, "iso", AccountState::Online).await;
    seed_event(&store, &ta, &acc_a, "iso").await;
    let acc_b = seed_account(&store, &tb, "iso", AccountState::Offline).await;
    for n in 0..3 {
        seed_event(&store, &tb, &acc_b, &format!("iso{n}")).await;
    }

    let ra = store
        .tenant_capacity(&ta, millis(4_000))
        .await
        .expect("report a");
    let rb = store
        .tenant_capacity(&tb, millis(4_000))
        .await
        .expect("report b");
    assert_eq!(
        (ra.accounts_total, ra.accounts_online, ra.events_total),
        (1, 1, 1),
        "tenant a sees only its rows"
    );
    assert_eq!(
        (rb.accounts_total, rb.accounts_online, rb.events_total),
        (1, 0, 3),
        "tenant b sees only its rows"
    );

    // The CLI face reports the same over a fresh connection.
    let reports = run_report(&url, &[ta.as_str().to_owned(), tb.as_str().to_owned()])
        .await
        .expect("cli report");
    assert_eq!(reports.len(), 2);
    assert_eq!(
        reports[0],
        TenantReport {
            tenant: ta.as_str().to_owned(),
            report: ra,
        }
    );
    assert_eq!(
        reports[1],
        TenantReport {
            tenant: tb.as_str().to_owned(),
            report: rb,
        }
    );
}

#[test]
fn render_names_the_tenant_and_stays_secret_free() {
    let line = TenantReport {
        tenant: "t-rep-demo".to_owned(),
        report: CapacityReport {
            accounts_total: 4,
            accounts_online: 3,
            workers_total: 2,
            workers_draining: 1,
            live_leases: 5,
            operations_pending: 2,
            operations_running: 1,
            operations_terminal: 9,
            events_total: 100,
            profiles_active: 2,
            sessions_active: 1,
            webhook_subscriptions: 1,
        },
    };
    let text = render(&[line]);
    assert!(
        text.contains("tenant=t-rep-demo accounts=4 online=3 workers=2 draining=1"),
        "{text}"
    );
    assert!(
        text.contains("live_leases=5 ops_pending=2 ops_running=1 ops_terminal=9"),
        "{text}"
    );
    assert!(
        text.contains("events=100 profiles=2 sessions=1 webhooks=1"),
        "{text}"
    );
    assert!(!text.contains("postgres://"), "{text}");
}
