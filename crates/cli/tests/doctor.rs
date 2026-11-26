//! F029 acceptance (doctor + migrate side): the read-only checks go
//! green on a current schema, renders stay secret-free, and the
//! migration job is idempotent. Database-gated like every DB-backed
//! suite.

use fishpond_cli::doctor::{DoctorCheck, DoctorReport, run_checks};
use fishpond_storage::{DbUrlSource, Store};

const TEST_DB_ENV: &str = "FISHPOND_TEST_DATABASE_URL";

fn test_db_url() -> Option<String> {
    std::env::var(TEST_DB_ENV)
        .ok()
        .filter(|v| !v.trim().is_empty())
}

#[tokio::test]
async fn doctor_goes_green_on_a_current_schema() {
    let Some(url) = test_db_url() else {
        eprintln!("skip: no database");
        return;
    };
    // Resolves through the FLAG path, so the test never touches the
    // FISHPOND_DATABASE_URL env names the pure tests use.
    let store = Store::connect(&url).await.expect("connect + migrate");

    let report = run_checks(&store, DbUrlSource::Flag).await;
    assert_eq!(report.checks.len(), 3);
    assert!(report.ok(), "{}", report.render());
    assert_eq!(report.checks[0].name, "database_url");
    assert_eq!(report.checks[1].name, "database_reachable");
    assert_eq!(report.checks[2].name, "schema_current");

    // The rendering is secret-free: the url value never appears.
    let rendered = report.render();
    assert!(rendered.contains("[ok]"));
    assert!(!rendered.contains("postgres://"), "{rendered}");
    assert!(
        !rendered.contains(&url),
        "the render must not echo the url: {rendered}"
    );
}

#[tokio::test]
async fn migrate_is_idempotent_and_schema_stays_current() {
    let Some(url) = test_db_url() else {
        eprintln!("skip: no database");
        return;
    };
    let store = Store::connect_unmigrated(&url).await.expect("connect");

    store
        .run_migrations()
        .await
        .expect("first run applies pending");
    let second = store.run_migrations().await.expect("second run");
    assert_eq!(second, 0, "a migrated schema has nothing left to apply");
    assert!(store.schema_current().await.expect("schema check"));
}

#[test]
fn reports_render_failures_honestly() {
    let report = DoctorReport {
        checks: vec![
            DoctorCheck {
                name: "database_url",
                ok: true,
                detail: "secret file from FISHPOND_DATABASE_URL_FILE (value withheld)".to_owned(),
            },
            DoctorCheck {
                name: "database_reachable",
                ok: false,
                detail: "connect failed: connection refused".to_owned(),
            },
        ],
    };
    assert!(!report.ok());
    let rendered = report.render();
    assert!(rendered.contains("[FAIL] database_reachable"), "{rendered}");
    assert!(rendered.contains("[ok] database_url"), "{rendered}");
}
