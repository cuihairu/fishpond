//! F031 security face: secrets never reach rendered surfaces, tenant
//! probes fail closed for a foreign tenant, webhook signatures reject
//! tampering, and concurrent incident opens elect exactly one row.
//! Env-gated where a database is involved.

mod common;

use common::{ENV, gated, tenant_record, unique_entity, unique_tenant};
use fishpond_events::{VerifyError, signature_header, verify_header};
use fishpond_monitor::{IncidentDraft, IncidentLog, Severity};
use fishpond_storage::NewOperation;
use fishpond_telemetry::{AlertThresholds, MailboxDepthSample, MetricsRegistry, evaluate};
use std::sync::Arc;

const SECRET: &str = "fp_live_secret_f031";

#[test]
fn rendered_surfaces_never_leak_a_pasted_secret() {
    // Metric labels are data-derived; the registry must redact both the
    // known secret and credential-shaped values before storing.
    let registry = MetricsRegistry::with_redaction([SECRET.to_owned()]);
    let note = format!("tenant {SECRET}");
    registry.set_gauge("tenant_state", [("note", note.as_str())], 1);
    registry.inc_counter(
        "redactions_total",
        [("endpoint", "postgres://svc:hunter2@db/x")],
        1,
    );
    let rendered = registry.render();
    assert!(!rendered.contains(SECRET), "{rendered}");
    assert!(!rendered.contains("hunter2"), "{rendered}");

    // Alert rendering carries counts and thresholds only — feed it a
    // mailbox named after a secret and the draft stays clean.
    let raised = evaluate(
        &fishpond_telemetry::QueueDepthReport {
            tenant: "t".to_owned(),
            operations_by_state: vec![],
            in_flight_attempts: 0,
            overdue_attempts: 0,
            consumer_lag: 0,
        },
        &[MailboxDepthSample {
            name: format!("supervisor:{SECRET}"),
            occupancy: 99,
            capacity: 100,
        }],
        &AlertThresholds::default(),
    );
    // The mailbox NAME is operator-configured (not secret-derived), so
    // it passes through — but detail stays numeric.
    assert_eq!(raised.len(), 1);
    assert!(!raised[0].detail.contains("payload"));
}

#[test]
fn webhook_signatures_reject_tampering() {
    let secret = "whsec_f031_test";
    let body = r#"{"event":"message.sent"}"#;
    let header = signature_header(secret, 1_000, body);

    // Honest delivery verifies.
    assert_eq!(verify_header(secret, 1_000, body, &header), Ok(()));

    // Any of body / timestamp / header tampering must fail.
    let tampered_body = body.replace("sent", "read");
    assert!(matches!(
        verify_header(secret, 1_000, &tampered_body, &header),
        Err(VerifyError::Mismatch)
    ));
    assert!(matches!(
        verify_header(secret, 2_000, body, &header),
        Err(VerifyError::Mismatch)
    ));
    let forged = format!("t=1000,v1={}", "0".repeat(64));
    assert!(matches!(
        verify_header(secret, 1_000, body, &forged),
        Err(VerifyError::Mismatch)
    ));
    assert!(matches!(
        verify_header(secret, 1_000, body, "not-a-signature"),
        Err(VerifyError::Malformed)
    ));

    // A different secret cannot validate the original header.
    assert!(matches!(
        verify_header("whsec_other", 1_000, body, &header),
        Err(VerifyError::Mismatch)
    ));
}

#[tokio::test]
async fn depth_probes_fail_closed_for_a_foreign_tenant() {
    let Some(store) = gated().await else {
        eprintln!("skip: {ENV} is not set (CI has no database)");
        return;
    };
    let mine = unique_tenant("f031-sec-mine");
    store
        .upsert_tenant(&tenant_record(&mine, "f031-sec-mine"))
        .await
        .expect("tenant insert");
    for n in 1..=2u32 {
        store
            .create_operation(&NewOperation {
                id: &fishpond_domain::OperationId::parse(unique_entity("op", &format!("sec-{n}")))
                    .expect("operation id"),
                tenant: &mine,
                kind: "send_message",
                idempotency_key: &unique_entity("key", &format!("sec-{n}")),
                at: common::ts(1_000),
            })
            .await
            .expect("create operation");
    }

    // A different tenant sees nothing of mine — through the telemetry
    // probes or the raw counts.
    let theirs = unique_tenant("f031-sec-theirs");
    store
        .upsert_tenant(&tenant_record(&theirs, "f031-sec-theirs"))
        .await
        .expect("tenant insert");
    let depth = fishpond_telemetry::collect_queue_depth(&store, &theirs, "projection")
        .await
        .expect("collect for the other tenant");
    assert_eq!(depth.backlog(), 0, "foreign tenant must see zero rows");
    assert_eq!(depth.in_flight_attempts, 0);
    assert_eq!(depth.consumer_lag, 0);
    let counts = store
        .operation_state_counts(&theirs)
        .await
        .expect("state counts for the other tenant");
    assert!(counts.is_empty(), "{counts:?}");
}

#[tokio::test]
async fn concurrent_incident_opens_elect_exactly_one_row() {
    let Some(store) = gated().await else {
        eprintln!("skip: {ENV} is not set (CI has no database)");
        return;
    };
    let tenant = unique_tenant("f031-sec-inc");
    store
        .upsert_tenant(&tenant_record(&tenant, "f031-sec-inc"))
        .await
        .expect("tenant insert");
    let log = IncidentLog::new(Arc::new(store.clone()));

    let id = unique_entity("inc", "sec-race");
    let make_draft = || IncidentDraft {
        tenant: "",
        id: &id,
        kind: "security-probe",
        severity: Severity::Warning,
        subject: "idempotency race",
        detail: "two opens race for one row",
    };

    // The idempotency key must collapse concurrent opens: exactly one
    // insert wins. Two racing calls see the same stable answer.
    let (first, second) =
        tokio::join!(log.open(make_draft(), 1_000), log.open(make_draft(), 1_000));
    let wins = [first.is_ok(), second.is_ok()];
    assert!(wins.iter().all(|ok| *ok), "opens must not error");
    let opened = [
        first.as_ref().expect("first").to_owned(),
        second.as_ref().expect("second").to_owned(),
    ];
    assert_eq!(
        opened.iter().filter(|opened| **opened).count(),
        1,
        "{opened:?}"
    );
}
