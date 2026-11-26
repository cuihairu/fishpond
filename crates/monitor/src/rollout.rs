//! F028 rollout audit: turns the versioned fleet's mutations into
//! operator incidents, so every promote/rollback/canary move leaves a
//! trail in the incident log (rollbacks are warnings — someone
//! retreated; the rest are info).

use crate::error::MonitorResult;
use crate::incident::{IncidentDraft, IncidentLog, Severity};
use fishpond_router::{FleetAction, FleetChange};

/// Records fleet mutations as incidents.
pub struct RolloutAuditor {
    log: IncidentLog,
}

impl RolloutAuditor {
    /// Builds the auditor over the shared incident log.
    pub fn new(log: IncidentLog) -> Self {
        Self { log }
    }

    /// Records one fleet change. The incident id folds in the action,
    /// platform and versions, so re-deriving the same change replays as
    /// a no-op; distinct moves open distinct incidents. Returns whether
    /// the incident was new.
    pub async fn record(&self, change: &FleetChange, now: i64) -> MonitorResult<bool> {
        let severity = if change.action == FleetAction::Rollback {
            Severity::Warning
        } else {
            Severity::Info
        };
        let from = change.from.as_deref().unwrap_or("-");
        let to = change.to.as_deref().unwrap_or("-");
        let id = format!(
            "rollout-{}-{}-from-{from}-to-{to}",
            change.action.as_str(),
            change.platform
        );
        let subject = format!(
            "{} rolled {} from {from} to {to}",
            change.platform,
            change.action.as_str()
        );
        self.log
            .open(
                IncidentDraft {
                    tenant: "",
                    id: &id,
                    kind: "rollout",
                    severity,
                    subject: &subject,
                    detail: "recorded by the rollout auditor",
                },
                now,
            )
            .await
    }

    /// Records every change in the trail; returns how many were new.
    pub async fn record_all(&self, changes: &[FleetChange], now: i64) -> MonitorResult<usize> {
        let mut fresh = 0;
        for change in changes {
            if self.record(change, now).await? {
                fresh += 1;
            }
        }
        Ok(fresh)
    }
}
