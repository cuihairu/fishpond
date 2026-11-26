//! F015 acceptance (storage side): the account observation audit trail.
//!
//! Every reconcile pass and manual confirmation appends one row; the
//! accounts row cannot carry a note on a confirmed state, so this table
//! is the only record of *why* an account moved. Covers append/read,
//! the UNKNOWN-note invariant, newest-first ordering and tenant
//! isolation (bare pool sees nothing).

mod common;

use common::{ENV, gated, tenant_record, ts, unique_entity, unique_tenant};
use fishpond_domain::{Account, AccountId, AccountState, TenantId, UnixMillis};
use fishpond_storage::{NewObservation, ObservationOutcome, ObservationSource};
use sqlx::Row;

/// An account of `tenant` in `state`, satisfying the UNKNOWN-note
/// invariant.
fn account(id: &str, tenant: &TenantId, state: AccountState) -> Account {
    let note = if state == AccountState::Unknown {
        Some("probe pending".to_owned())
    } else {
        None
    };
    Account::new(
        AccountId::parse(id).expect("account id is valid"),
        tenant.clone(),
        "mock",
        state,
        note,
        UnixMillis::from_millis(2_000).expect("non-negative"),
    )
    .expect("account satisfies its invariants")
}

#[allow(clippy::too_many_arguments)]
fn observation<'a>(
    id: &'a str,
    tenant: &'a TenantId,
    account: &'a AccountId,
    source: ObservationSource,
    observed: &'a str,
    outcome: ObservationOutcome,
    note: Option<&'a str>,
    operator: Option<&'a str>,
    at: i64,
) -> NewObservation<'a> {
    NewObservation {
        id,
        tenant,
        account,
        source,
        observed,
        outcome,
        note,
        operator,
        at: ts(at),
    }
}

#[tokio::test]
async fn observations_append_and_read_back_newest_first() {
    let Some(store) = gated().await else {
        eprintln!("skip: {ENV} is not set (CI has no database)");
        return;
    };
    let tenant = unique_tenant("obs-app");
    store
        .upsert_tenant(&tenant_record(&tenant, "obs-app"))
        .await
        .expect("tenant insert");
    let acc = AccountId::parse(unique_entity("acc", "obs-app")).expect("account id");
    store
        .upsert_account(&account(acc.as_str(), &tenant, AccountState::Offline))
        .await
        .expect("account insert");

    // Three observations in order: a probe flips the account, then a
    // probe leaves it unverifiable (note required), then a human
    // confirms it back.
    let first = unique_entity("obs", "obs-a");
    store
        .record_observation(&observation(
            &first,
            &tenant,
            &acc,
            ObservationSource::Reconcile,
            "ONLINE",
            ObservationOutcome::Applied,
            None,
            None,
            3_000,
        ))
        .await
        .expect("reconcile observation");
    let second = unique_entity("obs", "obs-b");
    store
        .record_observation(&observation(
            &second,
            &tenant,
            &acc,
            ObservationSource::Reconcile,
            "UNKNOWN",
            ObservationOutcome::Applied,
            Some("probe timed out"),
            None,
            4_000,
        ))
        .await
        .expect("unverifiable observation");
    let third = unique_entity("obs", "obs-c");
    store
        .record_observation(&observation(
            &third,
            &tenant,
            &acc,
            ObservationSource::Manual,
            "ONLINE",
            ObservationOutcome::Applied,
            Some("checked in the app"),
            Some("operator-a"),
            5_000,
        ))
        .await
        .expect("manual observation");

    let trail = store.list_observations(&tenant, &acc).await.expect("trail");
    assert_eq!(trail.len(), 3, "all three observations are recorded");
    assert_eq!(trail[0].id, third, "newest first");
    assert_eq!(trail[0].source, ObservationSource::Manual);
    assert_eq!(trail[0].operator.as_deref(), Some("operator-a"));
    assert_eq!(trail[1].id, second);
    assert_eq!(trail[1].observed, "UNKNOWN");
    assert_eq!(trail[1].note.as_deref(), Some("probe timed out"));
    assert_eq!(trail[2].id, first);
    assert_eq!(trail[2].outcome, ObservationOutcome::Applied);
}

#[tokio::test]
async fn unknown_observation_requires_a_note() {
    let Some(store) = gated().await else {
        eprintln!("skip: {ENV} is not set (CI has no database)");
        return;
    };
    let tenant = unique_tenant("obs-note");
    store
        .upsert_tenant(&tenant_record(&tenant, "obs-note"))
        .await
        .expect("tenant insert");
    let acc = AccountId::parse(unique_entity("acc", "obs-note")).expect("account id");
    store
        .upsert_account(&account(acc.as_str(), &tenant, AccountState::Online))
        .await
        .expect("account insert");

    // An UNKNOWN observation without a note would break the audit trail
    // the same way a noteless UNKNOWN account row breaks the domain.
    let err = store
        .record_observation(&observation(
            &unique_entity("obs", "note-1"),
            &tenant,
            &acc,
            ObservationSource::Reconcile,
            "UNKNOWN",
            ObservationOutcome::Applied,
            None,
            None,
            3_000,
        ))
        .await
        .expect_err("noteless UNKNOWN observation");
    assert!(
        err.to_string()
            .contains("required while the observed state is UNKNOWN"),
        "names the missing note: {err}"
    );
    // An empty note is as good as none.
    let err = store
        .record_observation(&observation(
            &unique_entity("obs", "note-2"),
            &tenant,
            &acc,
            ObservationSource::Reconcile,
            "UNKNOWN",
            ObservationOutcome::Applied,
            Some("   "),
            None,
            3_000,
        ))
        .await
        .expect_err("blank note");
    assert!(err.to_string().contains("UNKNOWN"));
}

#[tokio::test]
async fn observations_are_tenant_isolated() {
    let Some(store) = gated().await else {
        eprintln!("skip: {ENV} is not set (CI has no database)");
        return;
    };
    let tenant = unique_tenant("obs-iso-a");
    let other = unique_tenant("obs-iso-b");
    store
        .upsert_tenant(&tenant_record(&tenant, "obs-iso-a"))
        .await
        .expect("tenant A insert");
    store
        .upsert_tenant(&tenant_record(&other, "obs-iso-b"))
        .await
        .expect("tenant B insert");
    let acc = AccountId::parse(unique_entity("acc", "obs-iso")).expect("account id");
    store
        .upsert_account(&account(acc.as_str(), &tenant, AccountState::Offline))
        .await
        .expect("account insert");
    store
        .record_observation(&observation(
            &unique_entity("obs", "iso-1"),
            &tenant,
            &acc,
            ObservationSource::Reconcile,
            "OFFLINE",
            ObservationOutcome::Unchanged,
            None,
            None,
            3_000,
        ))
        .await
        .expect("observation");

    // The bare pool sees nothing of the tenant's observations.
    let bare: i64 = sqlx::query("SELECT count(*) FROM account_observations")
        .fetch_one(store.pool())
        .await
        .expect("bare count")
        .try_get("count")
        .expect("count column");
    assert_eq!(bare, 0, "no tenant rows leak to the bare pool");

    // A foreign tenant cannot read the trail; a foreign account id maps
    // to an empty trail, not an error.
    let cross = store
        .list_observations(&other, &acc)
        .await
        .expect("cross-tenant trail is empty");
    assert!(cross.is_empty());

    let mine = store
        .list_observations(&tenant, &acc)
        .await
        .expect("own trail");
    assert_eq!(mine.len(), 1);
}
