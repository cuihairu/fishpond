//! F039 acceptance (soak harness): the churn driver runs a *simulated*
//! stability horizon — virtual time advances one tick per iteration, so
//! a 72-hour horizon compresses into minutes of real time — over the
//! real database and the real scheduler/lease/operation machinery.
//! Every tick injects churn (node drains, worker deaths with takeover,
//! operation claim/close with mixed outcomes) and re-checks the safety
//! invariants: no live lease on a draining or dead worker, per-worker
//! capacity respected, fencing generations never move backwards. The
//! report is secret-free. Database-gated like every DB-backed suite.

use fishpond_cli::soak::{SoakConfig, render, run_soak};
use fishpond_domain::UnixMillis;
use fishpond_storage::{Store, TenantRecord};

const TEST_DB_ENV: &str = "FISHPOND_TEST_DATABASE_URL";

fn test_db_url() -> Option<String> {
    std::env::var(TEST_DB_ENV)
        .ok()
        .filter(|v| !v.trim().is_empty())
}

fn millis(value: i64) -> UnixMillis {
    UnixMillis::from_millis(value).expect("timestamp")
}

/// A churny one-simulated-hour soak at 30-second ticks: drains, worker
/// deaths and operation outcomes all fire, and no invariant ever
/// breaks.
#[tokio::test]
async fn soak_churn_survives_a_simulated_hour_without_violations() {
    let Some(url) = test_db_url() else {
        eprintln!("skip: no database");
        return;
    };
    let store = Store::connect(&url).await.expect("connect + migrate");
    let tenant = format!("f039-soak-{}-{}", std::process::id(), unique_suffix());
    store
        .upsert_tenant(&TenantRecord {
            id: fishpond_domain::TenantId::parse(tenant.clone()).expect("tenant id"),
            name: "f039-soak".to_owned(),
            created_at: millis(1_000),
        })
        .await
        .expect("tenant insert");

    let cfg = SoakConfig {
        sim_duration_ms: 3_600_000, // one simulated hour
        tick_ms: 30_000,            // 120 ticks
        nodes: 2,
        workers_per_node: 2,
        accounts: 6,
        capacity: 2,
        term_ms: 120_000,
        seed: 0xF039,
        real_cap_ms: 60_000,
    };
    let report = run_soak(&store, &tenant, &cfg)
        .await
        .expect("soak completes cleanly");
    eprintln!("{}", render(&report));
    assert_eq!(report.ticks, 120, "the horizon is fully simulated");
    assert!(
        report.violations.is_empty(),
        "safety invariants hold throughout: {:?}",
        report.violations
    );
    assert!(!report.time_capped, "the real-time cap never tripped");
    assert!(report.assigns > 0, "work was placed: {report:?}");
    assert!(
        report.operations_closed > 0,
        "operations churned: {report:?}"
    );
    assert!(
        report.node_drains > 0,
        "node drains fired during the run: {report:?}"
    );
    assert!(
        report.worker_deaths > 0,
        "worker deaths (takeover churn) fired: {report:?}"
    );
    assert!(
        report.moved > 0 || report.unassigned > 0,
        "drains and deaths moved (or tried to move) work: {report:?}"
    );
}

/// A zero horizon still performs one full setup+invariant pass: the
/// harness never reports success over nothing.
#[tokio::test]
async fn soak_zero_horizon_is_a_single_checked_pass() {
    let Some(url) = test_db_url() else {
        eprintln!("skip: no database");
        return;
    };
    let store = Store::connect(&url).await.expect("connect + migrate");
    let tenant = format!("f039-soak0-{}-{}", std::process::id(), unique_suffix());
    store
        .upsert_tenant(&TenantRecord {
            id: fishpond_domain::TenantId::parse(tenant.clone()).expect("tenant id"),
            name: "f039-soak0".to_owned(),
            created_at: millis(1_000),
        })
        .await
        .expect("tenant insert");
    let cfg = SoakConfig {
        sim_duration_ms: 0,
        ..SoakConfig::default()
    };
    let report = run_soak(&store, &tenant, &cfg)
        .await
        .expect("soak completes cleanly");
    assert_eq!(report.ticks, 1);
    assert!(report.violations.is_empty());
    assert!(report.assigns > 0, "the fleet was seeded and placed");
}

#[test]
fn render_is_secret_free_and_reports_the_counters() {
    let mut report = SoakConfig::default().empty_report();
    report.ticks = 120;
    report.assigns = 42;
    report.moved = 7;
    report.node_drains = 3;
    report.worker_deaths = 2;
    report.operations_claimed = 15;
    report.operations_closed = 15;
    report.violations.push("gen went backwards".to_owned());
    let text = render(&report);
    assert!(text.contains("ticks=120"), "{text}");
    assert!(text.contains("violations=1"), "{text}");
    assert!(!text.contains("postgres://"), "{text}");
}

/// Nanoseconds for per-run uniqueness (global primary keys, no cleanup).
fn unique_suffix() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_nanos()
}
