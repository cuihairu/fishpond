//! Resource alerts (F030 资源告警): threshold rules over a queue-depth
//! snapshot and mailbox samples, raising monitor incident drafts.
//! Evaluation is pure; opening the incident into the shared
//! [`fishpond_monitor::IncidentLog`] is the caller's move, so a firing
//! rule is visible in tests without a database.
//!
//! Incident ids are stable per rule (and subject), so repeated
//! evaluation of the same problem replays no-ops in the log.

use fishpond_monitor::{IncidentDraft, Severity};

use crate::depth::{MailboxDepthSample, QueueDepthReport};

/// Thresholds that separate "worth looking at" from "page someone".
/// Defaults are deliberately conservative — an empty system must be
/// quiet.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AlertThresholds {
    /// Non-terminal operations at or above this warn.
    pub backlog_warn: i64,
    /// Non-terminal operations at or above this are critical.
    pub backlog_critical: i64,
    /// Overdue attempts at or above this warn (one stuck attempt is
    /// already a symptom).
    pub overdue_warn: i64,
    /// Overdue attempts at or above this are critical.
    pub overdue_critical: i64,
    /// Consumer lag at or above this warns.
    pub lag_warn: u64,
    /// Consumer lag at or above this is critical.
    pub lag_critical: u64,
    /// Mailbox occupancy ratio at or above this warns.
    pub mailbox_warn_ratio: f64,
    /// Mailbox occupancy ratio at or above this is critical.
    pub mailbox_critical_ratio: f64,
}

impl Default for AlertThresholds {
    fn default() -> Self {
        Self {
            backlog_warn: 100,
            backlog_critical: 1_000,
            overdue_warn: 1,
            overdue_critical: 10,
            lag_warn: 1_000,
            lag_critical: 10_000,
            mailbox_warn_ratio: 0.8,
            mailbox_critical_ratio: 0.95,
        }
    }
}

/// One rule that fired.
#[derive(Debug, Clone, PartialEq)]
pub struct RaisedAlert {
    /// Stable rule name (part of the incident id).
    pub rule: &'static str,
    /// How loud.
    pub severity: Severity,
    /// What it is about (secret-free: rule + subject).
    pub subject: String,
    /// Stable incident id, computed at construction: `queue-{rule}`
    /// plus the subject when one exists, so replays collapse onto one
    /// row.
    pub incident_id: String,
    /// Secret-free detail: the numbers that fired, not the payloads.
    pub detail: String,
}

impl RaisedAlert {
    /// The monitor draft this alert opens (the id is stored, so the
    /// borrow is sound).
    pub fn draft<'a>(&'a self, tenant: &'a str) -> IncidentDraft<'a> {
        IncidentDraft {
            tenant,
            id: &self.incident_id,
            kind: "resource-alert",
            severity: self.severity,
            subject: &self.subject,
            detail: &self.detail,
        }
    }
}

/// Builds an alert, fixing its incident id from rule + subject.
fn alert(rule: &'static str, severity: Severity, subject: String, detail: String) -> RaisedAlert {
    let incident_id = if subject.is_empty() {
        format!("queue-{rule}")
    } else {
        format!("queue-{rule}-{subject}")
    };
    RaisedAlert {
        rule,
        severity,
        subject,
        incident_id,
        detail,
    }
}

/// Evaluates every rule; returns only the fires (a quiet system
/// returns nothing).
pub fn evaluate(
    report: &QueueDepthReport,
    mailboxes: &[MailboxDepthSample],
    thresholds: &AlertThresholds,
) -> Vec<RaisedAlert> {
    let mut alerts = Vec::new();

    let backlog = report.backlog();
    if backlog >= thresholds.backlog_critical {
        alerts.push(rule_alert(
            "operations-backlog",
            Severity::Critical,
            &report.tenant,
            backlog,
            thresholds.backlog_critical,
        ));
    } else if backlog >= thresholds.backlog_warn {
        alerts.push(rule_alert(
            "operations-backlog",
            Severity::Warning,
            &report.tenant,
            backlog,
            thresholds.backlog_warn,
        ));
    }

    if report.overdue_attempts >= thresholds.overdue_critical {
        alerts.push(rule_alert(
            "attempts-overdue",
            Severity::Critical,
            &report.tenant,
            report.overdue_attempts,
            thresholds.overdue_critical,
        ));
    } else if report.overdue_attempts >= thresholds.overdue_warn {
        alerts.push(rule_alert(
            "attempts-overdue",
            Severity::Warning,
            &report.tenant,
            report.overdue_attempts,
            thresholds.overdue_warn,
        ));
    }

    if report.consumer_lag >= thresholds.lag_critical {
        alerts.push(lag_alert(
            Severity::Critical,
            report,
            thresholds.lag_critical,
        ));
    } else if report.consumer_lag >= thresholds.lag_warn {
        alerts.push(lag_alert(Severity::Warning, report, thresholds.lag_warn));
    }

    for sample in mailboxes {
        let ratio = sample.ratio();
        let (severity, threshold) = if ratio >= thresholds.mailbox_critical_ratio {
            (Severity::Critical, thresholds.mailbox_critical_ratio)
        } else if ratio >= thresholds.mailbox_warn_ratio {
            (Severity::Warning, thresholds.mailbox_warn_ratio)
        } else {
            continue;
        };
        alerts.push(alert(
            "mailbox-saturation",
            severity,
            sample.name.clone(),
            format!(
                "occupancy {}/{} ({:.0}%) at or above {:.0}%",
                sample.occupancy,
                sample.capacity,
                ratio * 100.0,
                threshold * 100.0
            ),
        ));
    }

    tracing::debug!(alerts = alerts.len(), tenant = %report.tenant, "alert evaluation done");
    alerts
}

fn rule_alert(
    rule: &'static str,
    severity: Severity,
    tenant: &str,
    value: i64,
    threshold: i64,
) -> RaisedAlert {
    alert(
        rule,
        severity,
        tenant.to_owned(),
        format!("{rule}: {value} (threshold {threshold})"),
    )
}

fn lag_alert(severity: Severity, report: &QueueDepthReport, threshold: u64) -> RaisedAlert {
    alert(
        "consumer-lag",
        severity,
        report.tenant.clone(),
        format!(
            "consumer lag {} (threshold {})",
            report.consumer_lag, threshold
        ),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::depth::QueueDepthReport;

    fn report(
        states: Vec<(String, i64)>,
        in_flight: i64,
        overdue: i64,
        lag: u64,
    ) -> QueueDepthReport {
        QueueDepthReport {
            tenant: "t-acme".to_owned(),
            operations_by_state: states,
            in_flight_attempts: in_flight,
            overdue_attempts: overdue,
            consumer_lag: lag,
        }
    }

    #[test]
    fn a_healthy_system_is_quiet() {
        let quiet = report(vec![("PENDING".into(), 5)], 0, 0, 0);
        assert!(evaluate(&quiet, &[], &AlertThresholds::default()).is_empty());
    }

    #[test]
    fn backlog_and_overdue_fire_at_their_thresholds() {
        let thresholds = AlertThresholds::default();
        let busy = report(
            vec![("PENDING".into(), 150), ("RUNNING".into(), 12)],
            2,
            2,
            0,
        );
        let fires = evaluate(&busy, &[], &thresholds);
        assert_eq!(fires.len(), 2, "{fires:?}");
        assert!(fires.iter().all(|a| a.severity == Severity::Warning));
        assert!(
            fires
                .iter()
                .any(|a| a.rule == "operations-backlog" && a.subject == "t-acme")
        );

        let worse = report(vec![("PENDING".into(), 2_000)], 50, 20, 0);
        let fires = evaluate(&worse, &[], &thresholds);
        assert!(
            fires.iter().all(|a| a.severity == Severity::Critical),
            "{fires:?}"
        );
    }

    #[test]
    fn lag_escalates_with_depth() {
        let thresholds = AlertThresholds::default();
        let warn = report(vec![], 0, 0, 1_500);
        assert_eq!(
            evaluate(&warn, &[], &thresholds)
                .iter()
                .map(|a| a.severity)
                .collect::<Vec<_>>(),
            vec![Severity::Warning]
        );
        let crit = report(vec![], 0, 0, 20_000);
        assert_eq!(
            evaluate(&crit, &[], &thresholds)
                .iter()
                .map(|a| a.severity)
                .collect::<Vec<_>>(),
            vec![Severity::Critical]
        );
    }

    #[test]
    fn mailbox_ratio_rule_and_stable_ids() {
        let thresholds = AlertThresholds::default();
        let sample = MailboxDepthSample {
            name: "supervisor:acc-9".to_owned(),
            occupancy: 85,
            capacity: 100,
        };
        let fires = evaluate(
            &report(vec![], 0, 0, 0),
            std::slice::from_ref(&sample),
            &thresholds,
        );
        assert_eq!(fires.len(), 1, "{fires:?}");
        assert_eq!(fires[0].rule, "mailbox-saturation");
        assert_eq!(fires[0].severity, Severity::Warning);
        assert_eq!(
            fires[0].incident_id,
            "queue-mailbox-saturation-supervisor:acc-9"
        );

        // Same subject re-fires the same id (replay collapse).
        let again = evaluate(&report(vec![], 0, 0, 0), &[sample], &thresholds);
        assert_eq!(again[0].incident_id, fires[0].incident_id);
    }

    #[test]
    fn drafts_carry_no_payload_text() {
        let raised = RaisedAlert {
            rule: "operations-backlog",
            severity: Severity::Warning,
            subject: "t-acme".to_owned(),
            incident_id: "queue-operations-backlog-t-acme".to_owned(),
            detail: "operations-backlog: 150 (threshold 100)".to_owned(),
        };
        let draft = raised.draft("t-acme");
        assert_eq!(draft.kind, "resource-alert");
        assert!(!draft.detail.contains("payload"));
        assert!(
            draft
                .detail
                .chars()
                .all(|c| c.is_ascii_graphic() || c == ' ')
        );
    }
}
