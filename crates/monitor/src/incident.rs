//! F027 incident log: operator-visible problems, opened idempotently,
//! closed once, listed per tenant scope ('' = platform-wide).

use std::sync::Arc;

use fishpond_storage::{IncidentRecord, NewIncident, Store};

use crate::error::{MonitorError, MonitorResult};

/// How loud an incident is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    /// Worth noting; nothing is broken yet.
    Info,
    /// A capability claim is broken; traffic on it should stop.
    Warning,
    /// The platform is broken globally; all traffic should stop.
    Critical,
}

impl Severity {
    /// Canonical wire name.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Info => "info",
            Self::Warning => "warning",
            Self::Critical => "critical",
        }
    }

    /// Parses a canonical wire name.
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "info" => Some(Self::Info),
            "warning" => Some(Self::Warning),
            "critical" => Some(Self::Critical),
            _ => None,
        }
    }
}

/// The input to an idempotent open, minus the open instant (callers
/// pass it to [`IncidentLog::open`] so replays reuse their own clock).
#[derive(Debug, Clone, Copy)]
pub struct IncidentDraft<'a> {
    /// Owning tenant ('' = platform-wide).
    pub tenant: &'a str,
    /// Caller-chosen id; replays are no-ops.
    pub id: &'a str,
    /// The problem class (e.g. "contract-anomaly").
    pub kind: &'a str,
    /// How loud the incident is.
    pub severity: Severity,
    /// What the incident is about (secret-free).
    pub subject: &'a str,
    /// Free detail note (secret-free).
    pub detail: &'a str,
}

/// Opens and closes incidents.
pub struct IncidentLog {
    store: Arc<Store>,
}

impl IncidentLog {
    /// Builds the log over the shared store.
    pub fn new(store: Arc<Store>) -> Self {
        Self { store }
    }

    /// Opens an incident; replaying the same (tenant, id) is a no-op
    /// that returns false.
    pub async fn open(&self, draft: IncidentDraft<'_>, now: i64) -> MonitorResult<bool> {
        if draft.id.trim().is_empty() {
            return Err(MonitorError::EmptyId);
        }
        Ok(self
            .store
            .open_incident(&NewIncident {
                tenant: draft.tenant,
                id: draft.id,
                kind: draft.kind,
                severity: draft.severity.as_str(),
                subject: draft.subject,
                detail: draft.detail,
                opened_at: now,
            })
            .await?)
    }

    /// Closes an open incident; false when absent or already closed.
    pub async fn close(&self, tenant: &str, id: &str, now: i64) -> MonitorResult<bool> {
        if id.trim().is_empty() {
            return Err(MonitorError::EmptyId);
        }
        Ok(self.store.close_incident(tenant, id, now).await?)
    }

    /// One incident, if it exists in the scope.
    pub async fn get(&self, tenant: &str, id: &str) -> MonitorResult<Option<IncidentRecord>> {
        Ok(self.store.incident(tenant, id).await?)
    }

    /// The scope's incidents, newest first; `open_only` filters to the
    /// ones not yet closed.
    pub async fn list(&self, tenant: &str, open_only: bool) -> MonitorResult<Vec<IncidentRecord>> {
        Ok(self.store.list_incidents(tenant, open_only).await?)
    }

    /// The scope's still-open incidents, newest first.
    pub async fn open_incidents(&self, tenant: &str) -> MonitorResult<Vec<IncidentRecord>> {
        self.list(tenant, true).await
    }
}
