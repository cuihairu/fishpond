//! F018 acceptance (service side): subscribe behind the SSRF guard,
//! fan out with idempotency, sign deliveries, retry transient failures
//! on the backoff schedule, abandon permanent ones and exhausted
//! budgets, and replay (重投) on demand. The transport is a mock —
//! real HTTP is the F032+ red line.

use std::sync::Mutex;

use async_trait::async_trait;
use fishpond_domain::{AccountId, EventEnvelope, EventId, EventPayload, TenantId, UnixMillis};
use fishpond_events::{
    DeliveryReport, MAX_ATTEMPTS, TransportOutcome, WebhookService, WebhookTransport,
    signature_header, validate_endpoint, verify_header,
};
use fishpond_storage::{
    CredentialCipher, DeliveryStatus, MasterKey, SecretBox, Store, TenantRecord,
};

const ENV: &str = "FISHPOND_TEST_DATABASE_URL";
const SECRET: &str = "whsec_f018_service_signing_material";

fn ts(millis: i64) -> UnixMillis {
    UnixMillis::from_millis(millis).expect("non-negative millis")
}

fn unique_token(tag: &str) -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock is after the epoch")
        .as_nanos();
    format!("{tag}-{}-{nanos}", std::process::id())
}

async fn gated_service() -> Option<(Store, WebhookService)> {
    let url = std::env::var(ENV).ok().filter(|v| !v.trim().is_empty())?;
    let store = Store::connect(&url).await.expect("connect + migrate");
    let version = (std::process::id() as i32 % 2_000) * 1_000_000 + 424_242;
    store
        .register_master_key(version, ts(500))
        .await
        .expect("master key registration");
    let cipher = CredentialCipher::new(MasterKey::generate(), version);
    Some((store.clone(), WebhookService::new(store, cipher)))
}

async fn tenant_for(service: &WebhookService, tag: &str) -> TenantId {
    let tenant = TenantId::parse(unique_token(tag)).expect("tenant id is valid");
    service
        .store()
        .upsert_tenant(&TenantRecord {
            id: tenant.clone(),
            name: tag.to_owned(),
            created_at: ts(1_000),
        })
        .await
        .expect("tenant insert");
    tenant
}

async fn subscribe_all(
    service: &WebhookService,
    tenant: &TenantId,
    id: &str,
    url: &str,
    event_type: Option<&str>,
) {
    service
        .subscribe(
            tenant,
            id,
            url,
            SecretBox::new(SECRET),
            event_type,
            ts(1_100),
        )
        .await
        .expect("subscribe");
}

fn envelope(tenant: &TenantId, tag: &str, event_type: &str, seq: u64) -> EventEnvelope {
    EventEnvelope {
        id: EventId::parse(unique_token(tag)).expect("event id"),
        tenant_id: tenant.clone(),
        seq,
        event_type: event_type.to_owned(),
        account_id: Some(AccountId::parse("acc-iso-check").expect("account id")),
        occurred_at: ts(2_000),
        payload: EventPayload::parse(r#"{"kind":"sent","n":1}"#).expect("payload"),
    }
}

/// A transport that records every call and plays a scripted outcome
/// sequence (defaulting to success once the script runs out).
struct MockTransport {
    script: Mutex<Vec<TransportOutcome>>,
    calls: Mutex<Vec<RecordedCall>>,
}

#[derive(Clone)]
struct RecordedCall {
    url: String,
    headers: Vec<(String, String)>,
}

#[async_trait]
impl WebhookTransport for MockTransport {
    async fn deliver(
        &self,
        url: &str,
        headers: &[(&'static str, String)],
        _body: &str,
    ) -> TransportOutcome {
        self.calls.lock().expect("calls lock").push(RecordedCall {
            url: url.to_owned(),
            headers: headers
                .iter()
                .map(|(name, value)| ((*name).to_owned(), value.clone()))
                .collect(),
        });
        let mut script = self.script.lock().expect("script lock");
        if script.is_empty() {
            TransportOutcome::Success
        } else {
            script.remove(0)
        }
    }
}

impl MockTransport {
    fn new(script: Vec<TransportOutcome>) -> Self {
        Self {
            script: Mutex::new(script),
            calls: Mutex::new(Vec::new()),
        }
    }

    fn calls(&self) -> Vec<RecordedCall> {
        self.calls.lock().expect("calls lock").clone()
    }
}

#[tokio::test]
async fn subscribe_enforces_the_ssrf_guard() {
    let Some((_store, service)) = gated_service().await else {
        eprintln!("skip: {ENV} is not set (CI has no database)");
        return;
    };
    let tenant = tenant_for(&service, "wh18-ssrf").await;

    // Guard rejects before anything reaches storage.
    for endpoint in [
        "http://hooks.example.com/x",
        "https://127.0.0.1/hook",
        "https://169.254.169.254/meta",
        "https://localhost/hook",
        "https://user:pw@hooks.example.com/x",
    ] {
        let err = service
            .subscribe(
                &tenant,
                &unique_token("ssrf-bad"),
                endpoint,
                SecretBox::new(SECRET),
                None,
                ts(1_100),
            )
            .await
            .expect_err(endpoint);
        assert!(
            err.to_string().contains("webhook endpoint rejected"),
            "{endpoint}: {err}"
        );
        assert!(
            !err.to_string().contains("user:pw"),
            "userinfo never echoed"
        );
    }

    // A valid endpoint lands, sealed: fingerprint present, secret not.
    let id = unique_token("ssrf-ok");
    service
        .subscribe(
            &tenant,
            &id,
            "https://hooks.example.com/fishpond",
            SecretBox::new(SECRET),
            None,
            ts(1_100),
        )
        .await
        .expect("subscribe");
    let listed = service.list(&tenant).await.expect("list");
    let sub = listed.iter().find(|s| s.id == id).expect("stored");
    assert!(sub.is_enabled);
    let fingerprint = sub.fingerprint.clone().expect("fingerprint");
    assert!(!fingerprint.contains(SECRET));
    // The guard itself agrees the endpoint is fine.
    assert!(validate_endpoint("https://hooks.example.com/fishpond").is_ok());
}

#[tokio::test]
async fn enqueue_fans_out_idempotently_to_matching_subscriptions() {
    let Some((_store, service)) = gated_service().await else {
        eprintln!("skip: {ENV} is not set (CI has no database)");
        return;
    };
    let tenant = tenant_for(&service, "wh18-fan").await;
    subscribe_all(
        &service,
        &tenant,
        "sub-all",
        "https://hooks.example.com/all",
        None,
    )
    .await;
    subscribe_all(
        &service,
        &tenant,
        "sub-msg",
        "https://hooks.example.com/msg",
        Some("message.sent"),
    )
    .await;

    let message = envelope(&tenant, "fan-msg", "message.sent", 1);
    let ids = service
        .enqueue(&tenant, &message, ts(2_100))
        .await
        .expect("enqueue");
    assert_eq!(ids.len(), 2, "both subscriptions match");

    // Re-enqueuing the same event returns the same ids (idempotent
    // fan-out) — no duplicate rows.
    let again = service
        .enqueue(&tenant, &message, ts(2_200))
        .await
        .expect("re-enqueue");
    assert_eq!(again, ids);

    // A catalog event only hits the catch-all subscription.
    let catalog = envelope(&tenant, "fan-cat", "catalog.changed", 2);
    let ids = service
        .enqueue(&tenant, &catalog, ts(2_300))
        .await
        .expect("enqueue");
    assert_eq!(ids.len(), 1);

    // Disabling the catch-all shrinks the next fan-out to one.
    service
        .set_enabled(&tenant, "sub-all", false, ts(2_400))
        .await
        .expect("disable");
    let another = envelope(&tenant, "fan-msg2", "message.sent", 3);
    let ids = service
        .enqueue(&tenant, &another, ts(2_500))
        .await
        .expect("enqueue");
    assert_eq!(ids.len(), 1, "disabled subscriptions receive nothing");
}

#[tokio::test]
async fn delivery_signs_and_retries_on_the_backoff_schedule() {
    let Some((_store, service)) = gated_service().await else {
        eprintln!("skip: {ENV} is not set (CI has no database)");
        return;
    };
    let tenant = tenant_for(&service, "wh18-sign").await;
    subscribe_all(
        &service,
        &tenant,
        "sub-sign",
        "https://hooks.example.com/sign",
        None,
    )
    .await;

    let event = envelope(&tenant, "sign-evt", "message.sent", 1);
    let ids = service
        .enqueue(&tenant, &event, ts(1_000))
        .await
        .expect("enqueue");
    let delivery_id = ids.into_iter().next().expect("one delivery");
    let body = service
        .delivery(&tenant, &delivery_id)
        .await
        .expect("row")
        .body;
    assert!(
        body.contains("\"seq\":1"),
        "body snapshots the event: {body}"
    );

    // First pass: the endpoint 503s. The row parks for 30 s with the
    // outcome text persisted.
    let transport = MockTransport::new(vec![TransportOutcome::Transient {
        reason: "upstream 503".to_owned(),
    }]);
    let report = service
        .deliver_due(&tenant, &transport, ts(1_100), 60_000, 10)
        .await
        .expect("first pass");
    assert_eq!(
        report,
        DeliveryReport {
            delivered: 0,
            retried: 1,
            abandoned: 0
        }
    );
    let row = service.delivery(&tenant, &delivery_id).await.expect("row");
    assert_eq!(row.status, DeliveryStatus::Pending);
    assert_eq!(row.attempts, 1);
    assert_eq!(row.next_attempt_at, ts(1_100 + 30_000), "backoff attempt 1");
    assert_eq!(row.last_error.as_deref(), Some("upstream 503"));

    // The attempt carried a verifiable signature over the exact body.
    let sent = transport.calls();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].url, "https://hooks.example.com/sign");
    let sig = sent[0]
        .headers
        .iter()
        .find(|(name, _)| name == "x-fishpond-signature")
        .map(|(_, value)| value.clone())
        .expect("signature header");
    assert_eq!(
        verify_header(SECRET, 1_100, &body, &sig),
        Ok(()),
        "MAC verifies against the unsealed secret at the sent timestamp"
    );
    // And a re-sign at a different timestamp differs (fresh MAC).
    assert_ne!(sig, signature_header(SECRET, 1_200, &body));

    // Not due yet: nothing is claimed or sent.
    let transport = MockTransport::new(vec![]);
    let report = service
        .deliver_due(&tenant, &transport, ts(31_000), 60_000, 10)
        .await
        .expect("early pass");
    assert_eq!(report, DeliveryReport::default());
    assert!(transport.calls().is_empty());

    // Due now: the endpoint accepts. Status flips to delivered.
    let transport = MockTransport::new(vec![]);
    let report = service
        .deliver_due(&tenant, &transport, ts(31_100), 60_000, 10)
        .await
        .expect("second pass");
    assert_eq!(
        report,
        DeliveryReport {
            delivered: 1,
            retried: 0,
            abandoned: 0
        }
    );
    let row = service.delivery(&tenant, &delivery_id).await.expect("row");
    assert_eq!(row.status, DeliveryStatus::Delivered);
    assert!(row.last_error.is_none());
}

#[tokio::test]
async fn permanent_failures_abandon_and_budgets_exhaust() {
    let Some((_store, service)) = gated_service().await else {
        eprintln!("skip: {ENV} is not set (CI has no database)");
        return;
    };
    let tenant = tenant_for(&service, "wh18-perm").await;
    subscribe_all(
        &service,
        &tenant,
        "sub-perm",
        "https://hooks.example.com/perm",
        None,
    )
    .await;

    // A permanent fault abandons on the first attempt.
    let event = envelope(&tenant, "perm-evt", "message.sent", 1);
    let ids = service
        .enqueue(&tenant, &event, ts(1_000))
        .await
        .expect("enqueue");
    let id = ids.into_iter().next().expect("one delivery");
    let transport = MockTransport::new(vec![TransportOutcome::Permanent {
        reason: "upstream 410 gone".to_owned(),
    }]);
    let report = service
        .deliver_due(&tenant, &transport, ts(1_100), 60_000, 10)
        .await
        .expect("pass");
    assert_eq!(report.abandoned, 1);
    let row = service.delivery(&tenant, &id).await.expect("row");
    assert_eq!(row.status, DeliveryStatus::Abandoned);
    assert_eq!(row.last_error.as_deref(), Some("upstream 410 gone"));

    // A transient budget: each pass consumes one attempt until the
    // fifth failure abandons.
    let event = envelope(&tenant, "budget-evt", "message.sent", 2);
    let ids = service
        .enqueue(&tenant, &event, ts(1_000))
        .await
        .expect("enqueue");
    let id = ids.into_iter().next().expect("one delivery");
    // Script a transient outcome for every attempt; the last one still
    // returns transient but hits the exhausted budget.
    let transport = MockTransport::new(vec![
        TransportOutcome::Transient {
            reason: "upstream timeout".to_owned(),
        };
        MAX_ATTEMPTS as usize
    ]);
    let mut now = ts(1_100);
    for attempt in 1..MAX_ATTEMPTS {
        let report = service
            .deliver_due(&tenant, &transport, now, 60_000, 10)
            .await
            .expect("pass");
        assert_eq!(report.retried, 1, "attempt {attempt} retries");
        now = ts(now.as_millis() + fishpond_events::retry_delay_ms(attempt) as i64 + 1);
    }
    // Final (fifth) attempt: transient outcome but the budget is gone.
    let report = service
        .deliver_due(&tenant, &transport, now, 60_000, 10)
        .await
        .expect("final pass");
    assert_eq!(report.abandoned, 1, "budget exhausted → abandoned");
    let row = service.delivery(&tenant, &id).await.expect("row");
    assert_eq!(row.status, DeliveryStatus::Abandoned);
    assert_eq!(
        row.attempts,
        MAX_ATTEMPTS - 1,
        "the fifth attempt never stored"
    );
    assert!(!row.last_error.clone().unwrap_or_default().contains(SECRET));
}

#[tokio::test]
async fn replay_revives_a_budget_and_resends() {
    let Some((_store, service)) = gated_service().await else {
        eprintln!("skip: {ENV} is not set (CI has no database)");
        return;
    };
    let tenant = tenant_for(&service, "wh18-replay").await;
    subscribe_all(
        &service,
        &tenant,
        "sub-replay",
        "https://hooks.example.com/replay",
        None,
    )
    .await;

    let event = envelope(&tenant, "replay-evt", "message.sent", 1);
    let ids = service
        .enqueue(&tenant, &event, ts(1_000))
        .await
        .expect("enqueue");
    let id = ids.into_iter().next().expect("one delivery");
    let transport = MockTransport::new(vec![TransportOutcome::Permanent {
        reason: "upstream 410 gone".to_owned(),
    }]);
    service
        .deliver_due(&tenant, &transport, ts(1_100), 60_000, 10)
        .await
        .expect("abandoning pass");
    assert_eq!(
        service.delivery(&tenant, &id).await.expect("row").status,
        DeliveryStatus::Abandoned
    );

    // 重投: fresh budget, due immediately, and the endpoint now accepts.
    let transport = MockTransport::new(vec![]);
    service
        .replay(&tenant, &id, ts(9_000))
        .await
        .expect("replay");
    let row = service.delivery(&tenant, &id).await.expect("row");
    assert_eq!(row.status, DeliveryStatus::Pending);
    assert_eq!(row.attempts, 0);
    let report = service
        .deliver_due(&tenant, &transport, ts(9_100), 60_000, 10)
        .await
        .expect("post-replay pass");
    assert_eq!(report.delivered, 1);
    assert_eq!(
        service.delivery(&tenant, &id).await.expect("row").status,
        DeliveryStatus::Delivered
    );
}
