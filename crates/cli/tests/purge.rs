//! F040 acceptance (CLI purge): the operational sweep purges explicit
//! tenants through their tenant-bound transactions (an RLS-restricted
//! role cannot enumerate tenants — that fail-closed behavior is proven
//! in the storage suite). Dry runs roll back instead of committing, so
//! they report exactly what a real sweep would remove and change
//! nothing; renders never echo the url or any credential material.
//! Database-gated like every DB-backed suite.

use fishpond_cli::purge::{PurgeSummary, render, run_purge};
use fishpond_domain::{
    Account, AccountId, AccountState, AuthSessionId, AuthSessionState, TenantId, UnixMillis,
};
use fishpond_storage::{NewAuthSession, Store, TenantRecord};

const TEST_DB_ENV: &str = "FISHPOND_TEST_DATABASE_URL";

fn test_db_url() -> Option<String> {
    std::env::var(TEST_DB_ENV)
        .ok()
        .filter(|v| !v.trim().is_empty())
}

fn millis(value: i64) -> UnixMillis {
    UnixMillis::from_millis(value).expect("timestamp")
}

/// Seeds one tenant holding one expired session (stale) and one
/// authorized session (fresh); returns the ids.
async fn seed(store: &Store, tag: &str) -> (String, AuthSessionId, AuthSessionId) {
    // Per-run unique suffix so reruns never collide with leftovers.
    let unique = format!(
        "{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    );
    let tenant = TenantId::parse(format!("f040-purge-{tag}-{unique}")).expect("tenant id");
    store
        .upsert_tenant(&TenantRecord {
            id: tenant.clone(),
            name: tag.to_owned(),
            created_at: millis(1_000),
        })
        .await
        .expect("tenant insert");
    let account = Account::new(
        AccountId::parse(format!("acc-purge-{tag}-{unique}")).expect("account id"),
        tenant.clone(),
        "mock",
        AccountState::Online,
        None,
        millis(1_000),
    )
    .expect("account");
    store
        .upsert_account(&account)
        .await
        .expect("account insert");
    let stale = AuthSessionId::parse(format!("sess-purge-{tag}-{unique}-old")).expect("session id");
    let fresh = AuthSessionId::parse(format!("sess-purge-{tag}-{unique}-new")).expect("session id");
    for (id, state, at) in [
        (stale.clone(), AuthSessionState::Expired, 1_000),
        (fresh.clone(), AuthSessionState::Authorized, 8_000),
    ] {
        store
            .insert_auth_session(&NewAuthSession {
                id,
                tenant: tenant.clone(),
                account_id: account.id.clone(),
                platform: "mock".to_owned(),
                state,
                created_at: millis(at),
                expires_at: None,
            })
            .await
            .expect("session insert");
    }
    (tenant.as_str().to_owned(), stale, fresh)
}

#[tokio::test]
async fn purge_sweeps_stale_rows_only_after_dry_run() {
    let Some(url) = test_db_url() else {
        eprintln!("skip: no database");
        return;
    };
    let store = Store::connect(&url).await.expect("connect + migrate");
    let (tenant, stale, fresh) = seed(&store, "dryrun").await;
    let tenants = [tenant.clone()];

    // A dry run sees the stale session but leaves both rows untouched.
    let dry = run_purge(&url, millis(5_000), true, &tenants)
        .await
        .expect("dry run");
    assert_eq!(dry.tenants, 1, "{dry:?}");
    assert_eq!(dry.sessions, 1, "{dry:?}");
    let parsed = TenantId::parse(tenant.clone()).expect("tenant id");
    store
        .get_auth_session(&parsed, &stale)
        .await
        .expect("dry run changed nothing");
    store
        .get_auth_session(&parsed, &fresh)
        .await
        .expect("dry run kept the fresh session");

    // The real sweep removes exactly the stale row...
    let done = run_purge(&url, millis(5_000), false, &tenants)
        .await
        .expect("purge");
    assert_eq!(done.sessions, 1, "{done:?}");
    assert!(store.get_auth_session(&parsed, &stale).await.is_err());
    store
        .get_auth_session(&parsed, &fresh)
        .await
        .expect("fresh session survives the sweep");

    // ...and a second pass is a no-op.
    let again = run_purge(&url, millis(5_000), false, &tenants)
        .await
        .expect("second purge");
    assert_eq!(again.sessions, 0, "{again:?}");
    assert_eq!(again.grants, 0);
    assert_eq!(again.operations, 0);
    assert_eq!(again.deliveries, 0);
}

#[test]
fn render_is_secret_free_and_reports_dry_runs() {
    let summary = PurgeSummary {
        tenants: 3,
        sessions: 2,
        grants: 1,
        attempts: 4,
        operations: 2,
        deliveries: 5,
        dry_run: true,
    };
    let text = render(&summary);
    assert!(text.contains("dry run"), "{text}");
    assert!(text.contains("tenants=3"), "{text}");
    assert!(text.contains("sessions=2"), "{text}");
    assert!(!text.contains("postgres://"), "{text}");
}
