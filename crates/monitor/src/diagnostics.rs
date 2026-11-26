//! F027 contract-anomaly diagnostics: turns a provider contract run
//! into operator-visible anomalies, incidents and a health verdict.

use fishpond_provider_sdk::{
    Capability, ContractFixture, ContractReport, Provider, ProviderManifest, run_contract_suite,
};

use crate::error::MonitorResult;
use crate::health::{HealthRegistry, HealthStatus};
use crate::incident::{IncidentDraft, IncidentLog, Severity};

/// One contract violation attributed to a scope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContractAnomaly {
    /// The platform itself is broken (manifest, version negotiation) —
    /// every capability is suspect.
    Global {
        /// The failed check.
        check: &'static str,
        /// The violation description.
        detail: String,
    },
    /// One declared capability breaks its contract.
    Capability {
        /// The capability wire name (`Capability::as_str`).
        capability: &'static str,
        /// The failed check.
        check: &'static str,
        /// The violation description.
        detail: String,
    },
}

/// The suite checks each capability claim covers. Global checks
/// (`manifest_valid`, `version_negotiation`) are handled separately;
/// capabilities the suite does not exercise map to no checks.
fn checks_of(capability: Capability) -> &'static [&'static str] {
    match capability {
        Capability::Authorize => &["unauthorized_account_is_auth_error", "authorize_then_send"],
        Capability::SendMessage => &[
            "authorize_then_send",
            "idempotent_replay",
            "empty_text_rejected",
            "unknown_conversation_rejected",
        ],
        Capability::ReadConversations => &["conversations_listed", "history_oldest_first"],
        Capability::AccountState
        | Capability::ReadCatalog
        | Capability::ManageOrders
        | Capability::StreamEvents => &[],
    }
}

/// Suite checks that implicate the platform as a whole.
const GLOBAL_CHECKS: &[&str] = &["manifest_valid", "version_negotiation"];

/// Maps a contract report to anomalies, attributing each failed check
/// to the manifest's declared capabilities. A failed check no declared
/// capability covers is partial-provider suite noise — not an anomaly.
pub fn diagnose(report: &ContractReport, manifest: &ProviderManifest) -> Vec<ContractAnomaly> {
    let mut anomalies = Vec::new();
    for check in &report.checks {
        let Err(detail) = &check.outcome else {
            continue;
        };
        if GLOBAL_CHECKS.contains(&check.name) {
            anomalies.push(ContractAnomaly::Global {
                check: check.name,
                detail: detail.clone(),
            });
            continue;
        }
        for capability in &manifest.capabilities {
            if checks_of(*capability).contains(&check.name) {
                anomalies.push(ContractAnomaly::Capability {
                    capability: capability.as_str(),
                    check: check.name,
                    detail: detail.clone(),
                });
            }
        }
    }
    anomalies
}

/// Who/what/when of one probe run — the identity arguments of
/// [`audit`] and [`probe_platform`] folded into one struct.
#[derive(Debug, Clone, Copy)]
pub struct ProbeRun<'a> {
    /// The platform being probed.
    pub platform: &'a str,
    /// Tenant scope for opened incidents ('' = platform-wide).
    pub tenant: &'a str,
    /// Unique id of this run; incident ids fold it in.
    pub run_id: &'a str,
    /// Probe instant (millis since epoch).
    pub now: i64,
}

/// Records one incident per anomaly. Incident ids fold in the run id so
/// a rerun of the same broken platform opens fresh incidents instead of
/// replaying the old ones; within one run, replays stay no-ops. Global
/// anomalies open critical incidents, capability anomalies warnings.
/// Returns the anomalies that were audited.
pub async fn audit(
    log: &IncidentLog,
    report: &ContractReport,
    manifest: &ProviderManifest,
    run: ProbeRun<'_>,
) -> MonitorResult<Vec<ContractAnomaly>> {
    let ProbeRun {
        platform,
        tenant,
        run_id,
        now,
    } = run;
    let anomalies = diagnose(report, manifest);
    for anomaly in &anomalies {
        let (id, severity, subject, detail) = match anomaly {
            ContractAnomaly::Global { check, detail } => (
                format!("contract-{run_id}-{check}"),
                Severity::Critical,
                format!("{platform} contract broken globally: {check}"),
                detail.clone(),
            ),
            ContractAnomaly::Capability {
                capability,
                check,
                detail,
            } => (
                format!("contract-{run_id}-{check}-{capability}"),
                Severity::Warning,
                format!("{platform} contract broken for {capability}: {check}"),
                detail.clone(),
            ),
        };
        log.open(
            IncidentDraft {
                tenant,
                id: &id,
                kind: "contract-anomaly",
                severity,
                subject: &subject,
                detail: &detail,
            },
            now,
        )
        .await?;
    }
    Ok(anomalies)
}

/// Runs the contract suite against `provider`, audits incidents for any
/// anomaly and records the verdict as the platform's health: healthy on
/// a clean report, degraded on capability anomalies, down when a global
/// check breaks. Returns the recorded status.
pub async fn probe_platform(
    registry: &HealthRegistry,
    log: &IncidentLog,
    provider: &dyn Provider,
    fixture: &ContractFixture,
    run: ProbeRun<'_>,
) -> MonitorResult<HealthStatus> {
    let report = run_contract_suite(provider, fixture).await;
    let anomalies = audit(log, &report, provider.manifest(), run).await?;
    let (status, detail) = if anomalies
        .iter()
        .any(|a| matches!(a, ContractAnomaly::Global { .. }))
    {
        (
            HealthStatus::Down,
            "contract suite: global checks broken".to_owned(),
        )
    } else if anomalies.is_empty() {
        (HealthStatus::Healthy, "contract suite passed".to_owned())
    } else {
        let mut checks: Vec<&'static str> = anomalies
            .iter()
            .map(|a| match a {
                ContractAnomaly::Global { check, .. }
                | ContractAnomaly::Capability { check, .. } => *check,
            })
            .collect();
        checks.sort_unstable();
        checks.dedup();
        (
            HealthStatus::Degraded,
            format!(
                "contract suite: {} broken check(s): {}",
                checks.len(),
                checks.join(", ")
            ),
        )
    };
    registry
        .record(
            run.platform,
            &provider.manifest().provider_id,
            status,
            &detail,
            run.now,
        )
        .await?;
    Ok(status)
}
