//! F027 acceptance (storage side): platform health last-write-wins,
//! capability kill switches and the incident log's idempotent open /
//! once-only close / tenant-scoped listing. Skips without a database,
//! like every other DB-backed suite.

#[allow(dead_code)]
mod common;

use common::{gated, unique_token};
use fishpond_storage::{CapabilityOverride, HealthRecord, NewIncident};

fn health(platform: &str, status: &str, at: i64) -> HealthRecord {
    HealthRecord {
        platform: platform.to_owned(),
        provider_id: format!("prov-{platform}"),
        status: status.to_owned(),
        detail: format!("probe at {at}"),
        checked_at: at,
    }
}

fn override_row(platform: &str, capability: &str, disabled: bool, at: i64) -> CapabilityOverride {
    CapabilityOverride {
        platform: platform.to_owned(),
        capability: capability.to_owned(),
        disabled,
        reason: if disabled {
            "operator pause".to_owned()
        } else {
            String::new()
        },
        changed_at: at,
    }
}

/// Owned arguments for one incident, so the borrowed [`NewIncident`]
/// can point at live locals instead of leaked strings.
struct IncidentArgs {
    tenant: String,
    id: String,
    severity: String,
}

fn incident(args: &IncidentArgs, at: i64) -> NewIncident<'_> {
    NewIncident {
        tenant: &args.tenant,
        id: &args.id,
        kind: "test-problem",
        severity: &args.severity,
        subject: "storage test incident",
        detail: "from the storage suite",
        opened_at: at,
    }
}

#[tokio::test]
async fn health_upserts_are_last_write_wins() {
    let Some(store) = gated().await else {
        eprintln!("skip: no database");
        return;
    };
    let platform = unique_token("mon-h");
    store
        .upsert_health(&health(&platform, "healthy", 1_000))
        .await
        .expect("first probe");
    store
        .upsert_health(&health(&platform, "degraded", 2_000))
        .await
        .expect("second probe");

    let latest = store
        .health_of(&platform)
        .await
        .expect("read")
        .expect("exists");
    assert_eq!(latest.status, "degraded", "the newest probe wins");
    assert_eq!(latest.checked_at, 2_000);

    let all = store.list_health().await.expect("list");
    assert!(
        all.iter().any(|record| record.platform == platform),
        "the snapshot carries the platform"
    );
}

#[tokio::test]
async fn capability_overrides_flip_and_read_back() {
    let Some(store) = gated().await else {
        eprintln!("skip: no database");
        return;
    };
    let platform = unique_token("mon-c");
    assert!(
        store
            .find_capability_override(&platform, "conversations:write")
            .await
            .expect("find")
            .is_none(),
        "no decision yet — callers treat absent as allowed"
    );

    store
        .set_capability_override(&override_row(&platform, "conversations:write", true, 1_000))
        .await
        .expect("disable");
    let row = store
        .find_capability_override(&platform, "conversations:write")
        .await
        .expect("find")
        .expect("set");
    assert!(row.disabled && row.reason == "operator pause");

    store
        .set_capability_override(&override_row(
            &platform,
            "conversations:write",
            false,
            2_000,
        ))
        .await
        .expect("re-enable");
    let row = store
        .find_capability_override(&platform, "conversations:write")
        .await
        .expect("find")
        .expect("still recorded");
    assert!(!row.disabled, "the flip is the write");

    let all = store.list_capability_overrides().await.expect("list");
    assert!(all.iter().any(|row| row.platform == platform));
}

#[tokio::test]
async fn incidents_open_idempotently_and_close_once() {
    let Some(store) = gated().await else {
        eprintln!("skip: no database");
        return;
    };
    let tenant = unique_token("mon-i");
    let args = IncidentArgs {
        tenant: tenant.clone(),
        id: "inc-a".to_owned(),
        severity: "warning".to_owned(),
    };

    assert!(
        store
            .open_incident(&incident(&args, 1_000))
            .await
            .expect("open"),
        "first open is new"
    );
    assert!(
        !store
            .open_incident(&incident(&args, 1_100))
            .await
            .expect("replay"),
        "same id replays as a no-op (the original row stands)"
    );

    let listed = store
        .list_incidents(&tenant, true)
        .await
        .expect("open list");
    assert_eq!(listed.len(), 1, "{listed:?}");
    assert_eq!(listed[0].id, "inc-a");
    assert_eq!(listed[0].severity, "warning");
    assert_eq!(listed[0].opened_at, 1_000, "replay did not restamp");

    assert!(
        store
            .close_incident(&tenant, "inc-a", 2_000)
            .await
            .expect("close")
    );
    assert!(
        !store
            .close_incident(&tenant, "inc-a", 2_100)
            .await
            .expect("close again"),
        "a closed incident never re-closes"
    );
    assert!(
        store
            .list_incidents(&tenant, true)
            .await
            .expect("open list")
            .is_empty()
    );
    let closed = store
        .incident(&tenant, "inc-a")
        .await
        .expect("get")
        .expect("row stands");
    assert_eq!(closed.closed_at, Some(2_000));
    assert_eq!(
        store
            .list_incidents(&tenant, false)
            .await
            .expect("all")
            .len(),
        1
    );
}

#[tokio::test]
async fn incident_listings_never_cross_tenants() {
    let Some(store) = gated().await else {
        eprintln!("skip: no database");
        return;
    };
    let token = unique_token("mon-iso");
    let tenant_a = format!("t-a-{token}");
    let tenant_b = format!("t-b-{token}");
    let platform_wide = String::new();

    let a = IncidentArgs {
        tenant: tenant_a.clone(),
        id: "inc-x".to_owned(),
        severity: "info".to_owned(),
    };
    let b = IncidentArgs {
        tenant: tenant_b.clone(),
        id: "inc-y".to_owned(),
        severity: "info".to_owned(),
    };
    let wide = IncidentArgs {
        tenant: platform_wide.clone(),
        id: "inc-z".to_owned(),
        severity: "critical".to_owned(),
    };

    store
        .open_incident(&incident(&a, 1_000))
        .await
        .expect("open a");
    store
        .open_incident(&incident(&b, 1_100))
        .await
        .expect("open b");
    store
        .open_incident(&incident(&wide, 1_200))
        .await
        .expect("open platform-wide");

    let of_a = store
        .list_incidents(&tenant_a, false)
        .await
        .expect("list a");
    assert_eq!(of_a.len(), 1, "tenant a sees only its own: {of_a:?}");
    let of_b = store
        .list_incidents(&tenant_b, false)
        .await
        .expect("list b");
    assert_eq!(of_b.len(), 1);
    let scopeless = store
        .list_incidents(&platform_wide, false)
        .await
        .expect("list wide");
    assert_eq!(
        scopeless.len(),
        1,
        "the platform-wide scope is its own bucket"
    );
    assert!(
        store
            .incident(&tenant_a, "inc-y")
            .await
            .expect("get")
            .is_none(),
        "no cross-read across the tenant reference"
    );
}
