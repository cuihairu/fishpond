//! F027 acceptance (service side): health registry, capability
//! switchboard, incident log and the contract-anomaly probe. Pure tests
//! cover the status/severity vocabularies and the anomaly mapping;
//! database tests cover persistence and the clean/broken-stub probes.

#[allow(dead_code)]
mod common;

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use common::{gated, unique_token};
use fishpond_domain::{AccountId, Conversation, ConversationId, Message, Product, TenantId};
use fishpond_monitor::{
    CapabilitySwitchboard, ContractAnomaly, HealthRegistry, HealthStatus, IncidentDraft,
    IncidentLog, MonitorError, ProbeRun, Severity, audit, diagnose, probe_platform,
};
use fishpond_provider_mock::{MockProvider, SEED_ACCOUNT, SEED_CONVERSATION, SEED_TENANT};
use fishpond_provider_sdk::{
    AuthorizeOutcome, CallContext, Capability, CheckResult, ContractFixture, ContractReport,
    EngineKind, ErrorClass, ProtocolVersion, Provider, ProviderError, ProviderManifest,
    ProviderResult, SendMessageOutcome, SendMessageRequest,
};

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock is after the epoch")
        .as_millis() as i64
}

/// The fixture the mock provider promises to serve.
fn fixture() -> ContractFixture {
    ContractFixture {
        tenant: TenantId::parse(SEED_TENANT).expect("seed tenant parses"),
        account: AccountId::parse(SEED_ACCOUNT).expect("seed account parses"),
        conversation: ConversationId::parse(SEED_CONVERSATION).expect("seed conversation parses"),
    }
}

/// Every check the contract suite runs, in execution order.
const ALL_CHECKS: &[&str] = &[
    "manifest_valid",
    "version_negotiation",
    "authorize_then_send",
    "idempotent_replay",
    "history_oldest_first",
    "conversations_listed",
    "empty_text_rejected",
    "unknown_conversation_rejected",
    "unauthorized_account_is_auth_error",
];

/// A report over the full check list with only `failures` red.
fn synthetic_report(failures: &[&'static str]) -> ContractReport {
    let checks = ALL_CHECKS
        .iter()
        .map(|&name| CheckResult {
            name,
            outcome: if failures.contains(&name) {
                Err(format!("{name} violated"))
            } else {
                Ok(())
            },
        })
        .collect();
    ContractReport { checks }
}

/// A minimal manifest declaring exactly `capabilities`.
fn manifest_of(capabilities: &[Capability]) -> ProviderManifest {
    ProviderManifest {
        provider_id: "stub".to_owned(),
        display_name: "diagnose stub".to_owned(),
        provider_version: "1.0".to_owned(),
        engine: EngineKind::Api,
        protocol_versions: vec![ProtocolVersion::V1_0],
        capabilities: capabilities.to_vec(),
    }
}

// ---- pure: vocabularies ----

#[test]
fn status_and_severity_vocabularies_round_trip() {
    for status in [
        HealthStatus::Healthy,
        HealthStatus::Degraded,
        HealthStatus::Down,
        HealthStatus::Unknown,
    ] {
        assert_eq!(HealthStatus::parse(status.as_str()), Some(status));
    }
    assert_eq!(HealthStatus::parse("blazing"), None);

    for severity in [Severity::Info, Severity::Warning, Severity::Critical] {
        assert_eq!(Severity::parse(severity.as_str()), Some(severity));
    }
    assert_eq!(Severity::parse("loud"), None);
}

// ---- pure: anomaly mapping ----

#[test]
fn diagnose_attributes_failures_to_declared_capabilities() {
    // A global check failure is always a global anomaly.
    let anomalies = diagnose(
        &synthetic_report(&["manifest_valid"]),
        &manifest_of(&[Capability::Authorize]),
    );
    assert_eq!(
        anomalies,
        vec![ContractAnomaly::Global {
            check: "manifest_valid",
            detail: "manifest_valid violated".to_owned(),
        }]
    );

    // authorize_then_send implicates every capability that claims it.
    let anomalies = diagnose(
        &synthetic_report(&["authorize_then_send"]),
        &manifest_of(&[Capability::Authorize, Capability::SendMessage]),
    );
    assert_eq!(
        anomalies,
        vec![
            ContractAnomaly::Capability {
                capability: "accounts:authorize",
                check: "authorize_then_send",
                detail: "authorize_then_send violated".to_owned(),
            },
            ContractAnomaly::Capability {
                capability: "conversations:write",
                check: "authorize_then_send",
                detail: "authorize_then_send violated".to_owned(),
            },
        ]
    );

    // A failed check no declared capability covers is suite noise.
    let report = synthetic_report(&["conversations_listed"]);
    assert!(diagnose(&report, &manifest_of(&[Capability::SendMessage])).is_empty());
    let anomalies = diagnose(&report, &manifest_of(&[Capability::ReadConversations]));
    assert!(matches!(
        &anomalies[..],
        [ContractAnomaly::Capability {
            capability: "conversations:read",
            check: "conversations_listed",
            ..
        }]
    ));

    // A clean report diagnoses to nothing.
    assert!(
        diagnose(
            &synthetic_report(&[]),
            &manifest_of(&[Capability::SendMessage])
        )
        .is_empty()
    );
}

// ---- database: health registry ----

#[tokio::test]
async fn health_registry_keeps_the_last_probe() {
    let Some(store) = gated().await else {
        eprintln!("skip: no database");
        return;
    };
    let store = Arc::new(store);
    let registry = HealthRegistry::new(store);
    let platform = unique_token("f027-h");

    assert_eq!(
        registry.status_of(&platform).await.expect("status"),
        HealthStatus::Unknown,
        "never probed reads unknown"
    );

    registry
        .record(
            &platform,
            "prov-x",
            HealthStatus::Healthy,
            "first",
            now_ms(),
        )
        .await
        .expect("record");
    registry
        .record(
            &platform,
            "prov-x",
            HealthStatus::Degraded,
            "second",
            now_ms(),
        )
        .await
        .expect("record");
    assert_eq!(
        registry.status_of(&platform).await.expect("status"),
        HealthStatus::Degraded,
        "the newest probe wins"
    );

    let snapshot = registry.snapshot().await.expect("snapshot");
    assert!(
        snapshot
            .iter()
            .any(|r| r.platform == platform && r.status == "degraded"),
        "the snapshot carries the platform: {snapshot:?}"
    );
}

// ---- database: kill switch ----

#[tokio::test]
async fn switchboard_flips_the_kill_switch() {
    let Some(store) = gated().await else {
        eprintln!("skip: no database");
        return;
    };
    let store = Arc::new(store);
    let board = CapabilitySwitchboard::new(store);
    let platform = unique_token("f027-k");

    assert!(
        !board
            .denies(&platform, Capability::SendMessage)
            .await
            .expect("denies"),
        "no operator decision means allowed"
    );

    board
        .disable(
            &platform,
            Capability::SendMessage,
            "operator pause",
            now_ms(),
        )
        .await
        .expect("disable");
    assert!(
        board
            .denies(&platform, Capability::SendMessage)
            .await
            .expect("denies")
    );
    assert!(
        !board
            .denies(&platform, Capability::Authorize)
            .await
            .expect("denies"),
        "the switch is per capability"
    );

    board
        .enable(&platform, Capability::SendMessage, now_ms())
        .await
        .expect("enable");
    assert!(
        !board
            .denies(&platform, Capability::SendMessage)
            .await
            .expect("denies"),
        "the flip lifts the refusal"
    );

    let overrides = board.overrides().await.expect("overrides");
    assert!(
        overrides.iter().any(|row| row.platform == platform
            && row.capability == "conversations:write"
            && !row.disabled),
        "the lifted decision is still on record: {overrides:?}"
    );
}

// ---- database: incident log ----

#[tokio::test]
async fn incident_log_opens_once_and_closes_once() {
    let Some(store) = gated().await else {
        eprintln!("skip: no database");
        return;
    };
    let store = Arc::new(store);
    let log = IncidentLog::new(store);
    let tenant = unique_token("f027-i");
    let id = unique_token("f027-inc");

    assert!(
        log.open(
            IncidentDraft {
                tenant: &tenant,
                id: &id,
                kind: "manual",
                severity: Severity::Warning,
                subject: "subject",
                detail: "detail",
            },
            now_ms()
        )
        .await
        .expect("open"),
        "the first open is new"
    );
    assert!(
        !log.open(
            IncidentDraft {
                tenant: &tenant,
                id: &id,
                kind: "manual",
                severity: Severity::Critical,
                subject: "other",
                detail: "other",
            },
            now_ms()
        )
        .await
        .expect("replay"),
        "replays are no-ops"
    );

    let open = log.open_incidents(&tenant).await.expect("open list");
    assert_eq!(open.len(), 1);
    assert_eq!(open[0].severity, "warning", "the replay did not overwrite");
    assert_eq!(open[0].kind, "manual");

    assert!(log.close(&tenant, &id, now_ms()).await.expect("close"));
    assert!(
        !log.close(&tenant, &id, now_ms())
            .await
            .expect("close again"),
        "close is once-only"
    );
    assert!(
        log.open_incidents(&tenant)
            .await
            .expect("open list")
            .is_empty()
    );
    let closed = log
        .get(&tenant, &id)
        .await
        .expect("get")
        .expect("the row stands");
    assert!(closed.closed_at.is_some());

    assert!(
        matches!(
            log.open(
                IncidentDraft {
                    tenant: &tenant,
                    id: "   ",
                    kind: "manual",
                    severity: Severity::Info,
                    subject: "s",
                    detail: "d",
                },
                now_ms()
            )
            .await,
            Err(MonitorError::EmptyId)
        ),
        "blank incident ids are rejected"
    );
}

// ---- database: probe over the mock and a lying stub ----

/// Wraps the mock but breaks the send path while still declaring
/// `conversations:write` — the contradiction diagnostics must flag.
struct BrokenSender {
    inner: MockProvider,
}

#[async_trait]
impl Provider for BrokenSender {
    fn manifest(&self) -> &ProviderManifest {
        self.inner.manifest()
    }

    async fn authorize(&self, ctx: &CallContext) -> ProviderResult<AuthorizeOutcome> {
        self.inner.authorize(ctx).await
    }

    async fn send_message(
        &self,
        _ctx: &CallContext,
        _request: SendMessageRequest,
    ) -> ProviderResult<SendMessageOutcome> {
        Err(ProviderError::new(
            ErrorClass::Transient,
            "probe_broken",
            "send is broken by the test stub",
        ))
    }

    async fn list_conversations(&self, ctx: &CallContext) -> ProviderResult<Vec<Conversation>> {
        self.inner.list_conversations(ctx).await
    }

    async fn list_messages(
        &self,
        ctx: &CallContext,
        conversation: &ConversationId,
    ) -> ProviderResult<Vec<Message>> {
        self.inner.list_messages(ctx, conversation).await
    }

    async fn list_products(&self, ctx: &CallContext) -> ProviderResult<Vec<Product>> {
        self.inner.list_products(ctx).await
    }
}

#[tokio::test]
async fn probe_passes_clean_on_the_mock() {
    let Some(store) = gated().await else {
        eprintln!("skip: no database");
        return;
    };
    let store = Arc::new(store);
    let registry = HealthRegistry::new(store.clone());
    let log = IncidentLog::new(store);
    let platform = unique_token("f027-p");
    let tenant = unique_token("f027-clean");
    let provider = MockProvider::seeded();

    let status = probe_platform(
        &registry,
        &log,
        &provider,
        &fixture(),
        ProbeRun {
            platform: &platform,
            tenant: &tenant,
            run_id: &unique_token("run"),
            now: now_ms(),
        },
    )
    .await
    .expect("probe");
    assert_eq!(status, HealthStatus::Healthy);
    assert_eq!(
        registry.status_of(&platform).await.expect("status"),
        HealthStatus::Healthy
    );
    assert!(
        log.open_incidents(&tenant)
            .await
            .expect("incidents")
            .is_empty(),
        "a clean run opens nothing"
    );
}

#[tokio::test]
async fn probe_flags_a_broken_sender_and_opens_incidents() {
    let Some(store) = gated().await else {
        eprintln!("skip: no database");
        return;
    };
    let store = Arc::new(store);
    let registry = HealthRegistry::new(store.clone());
    let log = IncidentLog::new(store);
    let tenant = unique_token("f027-broken");
    let platform = unique_token("f027-b");
    let run_id = unique_token("run");
    let provider = BrokenSender {
        inner: MockProvider::seeded(),
    };

    let status = probe_platform(
        &registry,
        &log,
        &provider,
        &fixture(),
        ProbeRun {
            platform: &platform,
            tenant: &tenant,
            run_id: &run_id,
            now: now_ms(),
        },
    )
    .await
    .expect("probe");
    assert_eq!(
        status,
        HealthStatus::Degraded,
        "capability claims broke; the platform itself still answers"
    );
    assert_eq!(
        registry.status_of(&platform).await.expect("status"),
        HealthStatus::Degraded
    );

    let incidents = log.open_incidents(&tenant).await.expect("incidents");
    assert!(!incidents.is_empty(), "every anomaly opened an incident");
    assert!(
        incidents
            .iter()
            .all(|i| i.kind == "contract-anomaly" && i.severity == "warning"),
        "capability anomalies are warnings: {incidents:?}"
    );
    let ids: Vec<&str> = incidents.iter().map(|i| i.id.as_str()).collect();
    assert!(
        ids.iter().any(|id| id.contains("idempotent_replay")),
        "the broken send surfaces: {ids:?}"
    );
    assert!(
        ids.iter().any(|id| id.contains("authorize_then_send")),
        "{ids:?}"
    );

    // Rerunning the same run_id replays as no-ops; a fresh run opens new ones.
    let again_provider = BrokenSender {
        inner: MockProvider::seeded(),
    };
    probe_platform(
        &registry,
        &log,
        &again_provider,
        &fixture(),
        ProbeRun {
            platform: &platform,
            tenant: &tenant,
            run_id: &run_id,
            now: now_ms(),
        },
    )
    .await
    .expect("replay probe");
    assert_eq!(
        log.open_incidents(&tenant).await.expect("still open").len(),
        incidents.len(),
        "the same run id never doubles the incidents"
    );
    probe_platform(
        &registry,
        &log,
        &again_provider,
        &fixture(),
        ProbeRun {
            platform: &platform,
            tenant: &tenant,
            run_id: &unique_token("run"),
            now: now_ms(),
        },
    )
    .await
    .expect("fresh probe");
    let all = log.open_incidents(&tenant).await.expect("after fresh run");
    assert!(
        all.len() > incidents.len(),
        "a new run id opens fresh incidents: {} -> {}",
        incidents.len(),
        all.len()
    );

    // Close every open incident once; a second close is a no-op.
    for incident in &all {
        assert!(
            log.close(&tenant, &incident.id, now_ms())
                .await
                .expect("close")
        );
        assert!(
            !log.close(&tenant, &incident.id, now_ms())
                .await
                .expect("re-close")
        );
    }
    assert!(log.open_incidents(&tenant).await.expect("open").is_empty());
}

#[tokio::test]
async fn audit_opens_critical_incidents_for_global_breakage() {
    let Some(store) = gated().await else {
        eprintln!("skip: no database");
        return;
    };
    let store = Arc::new(store);
    let log = IncidentLog::new(store);
    let tenant = unique_token("f027-global");
    let run_id = unique_token("run");

    let anomalies = audit(
        &log,
        &synthetic_report(&["manifest_valid"]),
        &manifest_of(&[Capability::SendMessage]),
        ProbeRun {
            platform: "stub-platform",
            tenant: &tenant,
            run_id: &run_id,
            now: now_ms(),
        },
    )
    .await
    .expect("audit");
    assert!(matches!(
        &anomalies[..],
        [ContractAnomaly::Global {
            check: "manifest_valid",
            ..
        }]
    ));

    let open = log.open_incidents(&tenant).await.expect("open");
    assert_eq!(open.len(), 1);
    assert_eq!(open[0].severity, "critical", "global breakage is critical");
    assert_eq!(open[0].kind, "contract-anomaly");
    assert!(
        open[0].id.contains(&run_id),
        "the incident folds in the run id"
    );
}
