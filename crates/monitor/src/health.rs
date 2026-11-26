//! F027 health registration: the last probe result per platform,
//! last-write-wins, readable by operators without provider access.

use std::sync::Arc;

use fishpond_storage::{HealthRecord, Store};

use crate::error::MonitorResult;

/// Operator-readable probe outcome for one platform.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HealthStatus {
    /// The contract suite held on every declared capability.
    Healthy,
    /// Some capability claims are broken, but the platform answers.
    Degraded,
    /// The platform fails globally (manifest, version negotiation).
    Down,
    /// No probe has ever run (or the stored status is unparseable).
    Unknown,
}

impl HealthStatus {
    /// Canonical wire name.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Healthy => "healthy",
            Self::Degraded => "degraded",
            Self::Down => "down",
            Self::Unknown => "unknown",
        }
    }

    /// Parses a canonical wire name.
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "healthy" => Some(Self::Healthy),
            "degraded" => Some(Self::Degraded),
            "down" => Some(Self::Down),
            "unknown" => Some(Self::Unknown),
            _ => None,
        }
    }
}

/// Reads and writes platform health.
pub struct HealthRegistry {
    store: Arc<Store>,
}

impl HealthRegistry {
    /// Builds the registry over the shared store.
    pub fn new(store: Arc<Store>) -> Self {
        Self { store }
    }

    /// Records the latest probe result for the platform.
    pub async fn record(
        &self,
        platform: &str,
        provider_id: &str,
        status: HealthStatus,
        detail: &str,
        now: i64,
    ) -> MonitorResult<()> {
        self.store
            .upsert_health(&HealthRecord {
                platform: platform.to_owned(),
                provider_id: provider_id.to_owned(),
                status: status.as_str().to_owned(),
                detail: detail.to_owned(),
                checked_at: now,
            })
            .await?;
        Ok(())
    }

    /// Every platform's latest health, ordered by platform.
    pub async fn snapshot(&self) -> MonitorResult<Vec<HealthRecord>> {
        Ok(self.store.list_health().await?)
    }

    /// The platform's latest status; never-probed platforms read
    /// [`HealthStatus::Unknown`].
    pub async fn status_of(&self, platform: &str) -> MonitorResult<HealthStatus> {
        Ok(self
            .store
            .health_of(platform)
            .await?
            .and_then(|record| HealthStatus::parse(&record.status))
            .unwrap_or(HealthStatus::Unknown))
    }
}
