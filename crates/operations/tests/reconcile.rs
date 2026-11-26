//! F015 acceptance: the reconcile loop over real rows — deadline
//! reaping (including the worker-wins race), account probing
//! (resolve/unknown/refresh/skip) and manual verification (人工核对),
//! all through the domain state machines with the audit trail as the
//! record of *why* each account moved. Runs against the env-gated test
//! database, skipping when CI has none.

mod common;

use common::{ENV, account, gated, tenant_record, ts, unique_entity, unique_tenant};
use fishpond_domain::{AccountId, AccountState, AttemptId, OperationId, TenantId, WorkerId};
use fishpond_operations::{AccountProbe, ProbeOutcome, ReconcileError, ReconcileStep, Reconciler};
use fishpond_storage::{AttemptClose, NewOperation, ObservationSource, Store};
use std::sync::atomic::{AtomicUsize, Ordering};

/// A probe that always answers the same way, counting its calls so
/// tests can prove a disabled account was never probed.
struct FixedProbe {
    outcome: ProbeOutcome,
    calls: AtomicUsize,
}

impl FixedProbe {
    fn always(outcome: ProbeOutcome) -> Self {
        Self {
            outcome,
            calls: AtomicUsize::new(0),
        }
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

#[async_trait::async_trait]
impl AccountProbe for FixedProbe {
    async fn probe(&self, _account: &AccountId) -> ProbeOutcome {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.outcome.clone()
    }
}

fn operation_id(tag: &str, n: u32) -> OperationId {
    OperationId::parse(unique_entity("op", &format!("{tag}-{n}"))).expect("operation id")
}

fn attempt_id(tag: &str, n: u32) -> AttemptId {
    AttemptId::parse(unique_entity("att", &format!("{tag}-{n}"))).expect("attempt id")
}

fn worker_id(tag: &str, n: u32) -> WorkerId {
    WorkerId::parse(unique_entity("wrk", &format!("{tag}-{n}"))).expect("worker id")
}

/// Registers `tenant` with one account in `state`; returns both.
async fn setup_account(store: &Store, tag: &str, state: AccountState) -> (TenantId, AccountId) {
    let tenant = unique_tenant(tag);
    store
        .upsert_tenant(&tenant_record(&tenant, tag))
        .await
        .expect("tenant insert");
    let acc = AccountId::parse(unique_entity("acc", tag)).expect("account id");
    store
        .upsert_account(&account(acc.as_str(), &tenant, state))
        .await
        .expect("account insert");
    (tenant, acc)
}

/// Creates a PENDING operation under `tenant`.
async fn create_pending(
    store: &Store,
    tenant: &TenantId,
    tag: &str,
    n: u32,
    at: i64,
) -> OperationId {
    let id = operation_id(tag, n);
    let created = store
        .create_operation(&NewOperation {
            id: &id,
            tenant,
            kind: "send_message",
            idempotency_key: &unique_entity("key", &format!("{tag}-{n}")),
            at: ts(at),
        })
        .await
        .expect("create operation");
    created.operation.id
}

#[tokio::test]
async fn reap_closes_expired_attempts_and_reopens_operations() {
    let Some(store) = gated().await else {
        eprintln!("skip: {ENV} is not set (CI has no database)");
        return;
    };
    let tenant = unique_tenant("rc-reap");
    store
        .upsert_tenant(&tenant_record(&tenant, "rc-reap"))
        .await
        .expect("tenant insert");
    // Per-pass bound of 2 so the bounded-burst behavior is exercised.
    let reconciler = Reconciler::new(store.clone(), 2);
    let worker = worker_id("rc-reap", 1);

    // Three operations, each claimed and left in flight past its
    // deadline at t=4_000.
    let mut ops = Vec::new();
    for n in 1..=3u32 {
        let op = create_pending(&store, &tenant, "reap", n, 1_000).await;
        store
            .claim_operation(
                &tenant,
                &op,
                &worker,
                &attempt_id("reap", n),
                ts(3_000),
                ts(2_000),
            )
            .await
            .expect("claim");
        ops.push(op);
    }

    // One pass with a per-pass bound of 2 reaps two; the remainder is
    // picked up by the next pass (a burst cannot monopolize the loop).
    let report = reconciler
        .reap_deadlines(&tenant, ts(4_000))
        .await
        .expect("reap");
    assert_eq!(report.reaped.len(), 2, "bounded to max_reap_per_pass=2");
    assert!(report.contested.is_empty());

    let report = reconciler
        .reap_deadlines(&tenant, ts(4_000))
        .await
        .expect("reap again");
    assert_eq!(report.reaped.len(), 1);
    let rest = reconciler
        .reap_deadlines(&tenant, ts(4_000))
        .await
        .expect("reap third");
    assert!(rest.reaped.is_empty(), "idempotent: nothing left to reap");

    // Every reaped attempt is recorded TimedOut and its operation is
    // back to PENDING, ready for a later attempt.
    for op in &ops {
        let operation = store.get_operation(&tenant, op).await.expect("operation");
        assert_eq!(operation.state, fishpond_domain::OperationState::Pending);
        let attempts = store.list_attempts(&tenant, op).await.expect("attempts");
        assert_eq!(attempts.len(), 1);
        assert_eq!(
            attempts[0].outcome,
            Some(fishpond_domain::AttemptOutcome::TimedOut)
        );
    }
}

#[tokio::test]
async fn reap_reports_attempts_the_worker_closed_first() {
    let Some(store) = gated().await else {
        eprintln!("skip: {ENV} is not set (CI has no database)");
        return;
    };
    let tenant = unique_tenant("rc-contest");
    store
        .upsert_tenant(&tenant_record(&tenant, "rc-contest"))
        .await
        .expect("tenant insert");
    let reconciler = Reconciler::new(store.clone(), 10);
    let worker = worker_id("rc-contest", 1);

    let op = create_pending(&store, &tenant, "contest", 1, 1_000).await;
    let claim = store
        .claim_operation(
            &tenant,
            &op,
            &worker,
            &attempt_id("contest", 1),
            ts(3_000),
            ts(2_000),
        )
        .await
        .expect("claim");

    // The reaper reads its snapshot; *before it closes anything*, the
    // worker finishes the work and its completion CAS wins. Reaping the
    // stale snapshot records the attempt as contested instead of
    // fighting the live result.
    let snapshot = store
        .expired_attempts(&tenant, ts(4_000))
        .await
        .expect("scan");
    assert_eq!(snapshot.len(), 1);
    store
        .close_attempt(
            &tenant,
            &op,
            &claim.attempt.attempt_id,
            &worker,
            AttemptClose::Succeeded,
            ts(3_500),
        )
        .await
        .expect("worker completes first");

    let report = reconciler
        .reap_expired(&tenant, snapshot, ts(4_000))
        .await
        .expect("reap stale snapshot");
    assert!(report.reaped.is_empty());
    assert_eq!(report.contested.len(), 1);
    assert_eq!(report.contested[0].attempt, claim.attempt.attempt_id);

    // The worker's outcome stands: SUCCEEDED, terminal — the reaper
    // never overwrote it.
    let operation = store.get_operation(&tenant, &op).await.expect("operation");
    assert_eq!(operation.state, fishpond_domain::OperationState::Succeeded);
}

#[tokio::test]
async fn probe_resolves_unknown_and_marks_unverifiable() {
    let Some(store) = gated().await else {
        eprintln!("skip: {ENV} is not set (CI has no database)");
        return;
    };
    let (tenant, acc) = setup_account(&store, "rc-probe", AccountState::Unknown).await;
    let reconciler = Reconciler::new(store.clone(), 10);

    // A confirming probe resolves UNKNOWN → ONLINE and clears the note.
    let confirmed = FixedProbe::always(ProbeOutcome::Confirmed(AccountState::Online));
    let step = reconciler
        .probe_account(
            &tenant,
            &acc,
            &confirmed,
            &unique_entity("obs", "p1"),
            ts(3_000),
        )
        .await
        .expect("probe 1");
    assert_eq!(
        step,
        ReconcileStep::Applied {
            from: AccountState::Unknown,
            to: AccountState::Online,
        }
    );
    let account = store.get_account(&tenant, &acc).await.expect("account");
    assert_eq!(account.state, AccountState::Online);
    assert_eq!(account.state_note, None, "confirmed states carry no note");

    // An unverifiable probe moves ONLINE → UNKNOWN with the reason as
    // the note (never a silent online/offline guess).
    let flaky = FixedProbe::always(ProbeOutcome::Unverifiable {
        reason: "upstream timed out".to_owned(),
    });
    let step = reconciler
        .probe_account(
            &tenant,
            &acc,
            &flaky,
            &unique_entity("obs", "p2"),
            ts(4_000),
        )
        .await
        .expect("probe 2");
    assert_eq!(
        step,
        ReconcileStep::Applied {
            from: AccountState::Online,
            to: AccountState::Unknown,
        }
    );
    let account = store.get_account(&tenant, &acc).await.expect("account");
    assert_eq!(account.state, AccountState::Unknown);
    assert_eq!(account.state_note.as_deref(), Some("upstream timed out"));

    // Probing an already-UNKNOWN account with another unverifiable
    // result is Unchanged (no transition), but the note is refreshed to
    // the newest reason.
    let step = reconciler
        .probe_account(
            &tenant,
            &acc,
            &flaky,
            &unique_entity("obs", "p3"),
            ts(5_000),
        )
        .await
        .expect("probe 3");
    assert_eq!(step, ReconcileStep::Unchanged(AccountState::Unknown));
    let account = store.get_account(&tenant, &acc).await.expect("account");
    assert_eq!(account.state_note.as_deref(), Some("upstream timed out"));
    let fresher = FixedProbe::always(ProbeOutcome::Unverifiable {
        reason: "gateway 502".to_owned(),
    });
    let step = reconciler
        .probe_account(
            &tenant,
            &acc,
            &fresher,
            &unique_entity("obs", "p4"),
            ts(6_000),
        )
        .await
        .expect("probe 4");
    assert_eq!(step, ReconcileStep::Unchanged(AccountState::Unknown));
    let account = store.get_account(&tenant, &acc).await.expect("account");
    assert_eq!(
        account.state_note.as_deref(),
        Some("gateway 502"),
        "note refreshed"
    );

    // Confirming an already-confirmed state is Unchanged, not a no-op
    // transition error.
    let confirmed = FixedProbe::always(ProbeOutcome::Confirmed(AccountState::Offline));
    let step = reconciler
        .probe_account(
            &tenant,
            &acc,
            &confirmed,
            &unique_entity("obs", "p5"),
            ts(7_000),
        )
        .await
        .expect("probe 5");
    assert_eq!(
        step,
        ReconcileStep::Applied {
            from: AccountState::Unknown,
            to: AccountState::Offline,
        }
    );
    let step = reconciler
        .probe_account(
            &tenant,
            &acc,
            &confirmed,
            &unique_entity("obs", "p6"),
            ts(8_000),
        )
        .await
        .expect("probe 6");
    assert_eq!(step, ReconcileStep::Unchanged(AccountState::Offline));

    // The audit trail has every probe, oldest → newest.
    let trail = store
        .list_observations(&tenant, &acc)
        .await
        .expect("audit trail");
    assert_eq!(trail.len(), 6);
    assert!(
        trail
            .iter()
            .all(|row| row.source == ObservationSource::Reconcile)
    );
    assert_eq!(trail[0].observed, "OFFLINE", "newest first");
    assert_eq!(trail[5].observed, "ONLINE");
    assert_eq!(trail[4].note.as_deref(), Some("upstream timed out"));
}

#[tokio::test]
async fn probe_skips_disabled_without_calling_the_probe() {
    let Some(store) = gated().await else {
        eprintln!("skip: {ENV} is not set (CI has no database)");
        return;
    };
    let (tenant, acc) = setup_account(&store, "rc-skip", AccountState::Disabled).await;
    let reconciler = Reconciler::new(store.clone(), 10);
    let probe = FixedProbe::always(ProbeOutcome::Confirmed(AccountState::Online));

    let step = reconciler
        .probe_account(
            &tenant,
            &acc,
            &probe,
            &unique_entity("obs", "s1"),
            ts(3_000),
        )
        .await
        .expect("probe disabled");
    assert_eq!(step, ReconcileStep::SkippedDisabled);
    assert_eq!(probe.calls(), 0, "a disabled account is never probed");

    // The skip is audited, so a silent skip is distinguishable from a
    // missed scan.
    let trail = store
        .list_observations(&tenant, &acc)
        .await
        .expect("audit trail");
    assert_eq!(trail.len(), 1);
    assert_eq!(
        trail[0].outcome,
        fishpond_storage::ObservationOutcome::Skipped
    );

    // The account state is untouched.
    let account = store.get_account(&tenant, &acc).await.expect("account");
    assert_eq!(account.state, AccountState::Disabled);
}

#[tokio::test]
async fn probe_rejects_bogus_outcomes() {
    let Some(store) = gated().await else {
        eprintln!("skip: {ENV} is not set (CI has no database)");
        return;
    };
    let (tenant, acc) = setup_account(&store, "rc-bogus", AccountState::Online).await;
    let reconciler = Reconciler::new(store.clone(), 10);

    // A probe cannot "confirm" UNKNOWN or DISABLED — those are not
    // probe-verifiable conclusions.
    let liar = FixedProbe::always(ProbeOutcome::Confirmed(AccountState::Unknown));
    let err = reconciler
        .probe_account(&tenant, &acc, &liar, &unique_entity("obs", "b1"), ts(3_000))
        .await
        .expect_err("confirmed UNKNOWN is rejected");
    assert!(
        err.to_string().contains("not a probe-verifiable state"),
        "{err}"
    );

    // An unverifiable probe must say why — a reasonless UNKNOWN would
    // break the domain note invariant one layer down.
    let mute = FixedProbe::always(ProbeOutcome::Unverifiable {
        reason: "  ".to_owned(),
    });
    let err = reconciler
        .probe_account(&tenant, &acc, &mute, &unique_entity("obs", "b2"), ts(3_000))
        .await
        .expect_err("blank reason is rejected");
    assert!(err.to_string().contains("must say why"), "{err}");

    // Neither rejection touched the account or the audit trail.
    let account = store.get_account(&tenant, &acc).await.expect("account");
    assert_eq!(account.state, AccountState::Online);
    let trail = store
        .list_observations(&tenant, &acc)
        .await
        .expect("audit trail");
    assert!(trail.is_empty(), "rejected probes are not recorded");
}

#[tokio::test]
async fn manual_verification_applies_and_audits_the_operator() {
    let Some(store) = gated().await else {
        eprintln!("skip: {ENV} is not set (CI has no database)");
        return;
    };
    let (tenant, acc) = setup_account(&store, "rc-manual", AccountState::Unknown).await;
    let reconciler = Reconciler::new(store.clone(), 10);

    // A human confirms ONLINE with a justification; the live row keeps
    // no note (domain invariant), the audit trail carries who and why.
    let step = reconciler
        .confirm_manual(
            &tenant,
            &acc,
            AccountState::Online,
            "operator-a",
            Some("verified on the phone"),
            &unique_entity("obs", "m1"),
            ts(3_000),
        )
        .await
        .expect("manual confirm");
    assert_eq!(
        step,
        ReconcileStep::Applied {
            from: AccountState::Unknown,
            to: AccountState::Online,
        }
    );
    let account = store.get_account(&tenant, &acc).await.expect("account");
    assert_eq!(account.state, AccountState::Online);
    assert_eq!(account.state_note, None);

    // Confirming the state the account already has is an Unchanged
    // verification, still audited.
    let step = reconciler
        .confirm_manual(
            &tenant,
            &acc,
            AccountState::Online,
            "operator-b",
            None,
            &unique_entity("obs", "m2"),
            ts(4_000),
        )
        .await
        .expect("manual re-confirm");
    assert_eq!(step, ReconcileStep::Unchanged(AccountState::Online));

    let trail = store
        .list_observations(&tenant, &acc)
        .await
        .expect("audit trail");
    assert_eq!(trail.len(), 2);
    assert_eq!(trail[0].source, ObservationSource::Manual);
    assert_eq!(trail[0].operator.as_deref(), Some("operator-b"));
    assert_eq!(
        trail[0].outcome,
        fishpond_storage::ObservationOutcome::Unchanged
    );
    assert_eq!(trail[1].source, ObservationSource::Manual);
    assert_eq!(trail[1].operator.as_deref(), Some("operator-a"));
    assert_eq!(trail[1].note.as_deref(), Some("verified on the phone"));
    assert_eq!(
        trail[1].outcome,
        fishpond_storage::ObservationOutcome::Applied
    );
}

#[tokio::test]
async fn manual_rejects_unknown_empty_operator_and_disabled() {
    let Some(store) = gated().await else {
        eprintln!("skip: {ENV} is not set (CI has no database)");
        return;
    };
    let (tenant, acc) = setup_account(&store, "rc-mbad", AccountState::Online).await;
    let disabled_tenant = {
        let (t, a) = setup_account(&store, "rc-mdis", AccountState::Disabled).await;
        (t, a)
    };
    let reconciler = Reconciler::new(store.clone(), 10);

    // A human asserting "unverified" is not a verification.
    let err = reconciler
        .confirm_manual(
            &tenant,
            &acc,
            AccountState::Unknown,
            "operator-a",
            None,
            &unique_entity("obs", "mb1"),
            ts(3_000),
        )
        .await
        .expect_err("manual UNKNOWN");
    assert!(err.to_string().contains("not a confirmed state"), "{err}");

    // An unnamed operator cannot drive the audit trail.
    let err = reconciler
        .confirm_manual(
            &tenant,
            &acc,
            AccountState::Offline,
            "  ",
            None,
            &unique_entity("obs", "mb2"),
            ts(3_000),
        )
        .await
        .expect_err("anonymous operator");
    assert!(err.to_string().contains("must name who verified"), "{err}");

    // DISABLED is terminal: even a human cannot transition out of it
    // through verification.
    let err = reconciler
        .confirm_manual(
            &disabled_tenant.0,
            &disabled_tenant.1,
            AccountState::Online,
            "operator-a",
            None,
            &unique_entity("obs", "mb3"),
            ts(3_000),
        )
        .await
        .expect_err("manual out of DISABLED");
    assert!(err.to_string().contains("disabled"), "{err}");

    // None of the rejected calls touched state or trail.
    let account = store.get_account(&tenant, &acc).await.expect("account");
    assert_eq!(account.state, AccountState::Online);
    let trail = store
        .list_observations(&tenant, &acc)
        .await
        .expect("audit trail");
    assert!(trail.is_empty());
}

#[tokio::test]
async fn reconcile_is_tenant_isolated() {
    let Some(store) = gated().await else {
        eprintln!("skip: {ENV} is not set (CI has no database)");
        return;
    };
    let (tenant, acc) = setup_account(&store, "rc-iso-a", AccountState::Unknown).await;
    let (other, _) = setup_account(&store, "rc-iso-b", AccountState::Online).await;
    let reconciler = Reconciler::new(store.clone(), 10);
    let probe = FixedProbe::always(ProbeOutcome::Confirmed(AccountState::Online));

    // A foreign tenant cannot probe, confirm or reap this tenant's rows
    // — they are invisible, i.e. not found.
    let err = reconciler
        .probe_account(&other, &acc, &probe, &unique_entity("obs", "i1"), ts(3_000))
        .await
        .expect_err("cross-tenant probe");
    assert!(
        matches!(err, ReconcileError::NotFound { .. }),
        "got {err:?}"
    );

    let err = reconciler
        .confirm_manual(
            &other,
            &acc,
            AccountState::Online,
            "operator-a",
            None,
            &unique_entity("obs", "i2"),
            ts(3_000),
        )
        .await
        .expect_err("cross-tenant confirm");
    assert!(
        matches!(err, ReconcileError::NotFound { .. }),
        "got {err:?}"
    );

    let report = reconciler
        .reap_deadlines(&other, ts(3_000))
        .await
        .expect("foreign reap");
    assert!(report.reaped.is_empty() && report.contested.is_empty());

    // The owning tenant is unaffected: still UNKNOWN, still unaudited.
    let account = store.get_account(&tenant, &acc).await.expect("account");
    assert_eq!(account.state, AccountState::Unknown);
    let trail = store
        .list_observations(&tenant, &acc)
        .await
        .expect("audit trail");
    assert!(trail.is_empty());
}
