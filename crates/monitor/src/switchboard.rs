//! F027 capability kill switch: an operator-level refusal per
//! (platform, capability) that outranks scopes and manifest support.

use std::sync::Arc;

use fishpond_provider_sdk::Capability;
use fishpond_storage::{CapabilityOverride, Store};

use crate::error::MonitorResult;

/// Reads and flips capability overrides.
pub struct CapabilitySwitchboard {
    store: Arc<Store>,
}

impl CapabilitySwitchboard {
    /// Builds the switchboard over the shared store.
    pub fn new(store: Arc<Store>) -> Self {
        Self { store }
    }

    /// Refuses new calls for (platform, capability) with a reason.
    pub async fn disable(
        &self,
        platform: &str,
        capability: Capability,
        reason: &str,
        now: i64,
    ) -> MonitorResult<()> {
        self.store
            .set_capability_override(&CapabilityOverride {
                platform: platform.to_owned(),
                capability: capability.as_str().to_owned(),
                disabled: true,
                reason: reason.to_owned(),
                changed_at: now,
            })
            .await?;
        Ok(())
    }

    /// Lifts the refusal — the flip is the write, the reason clears.
    pub async fn enable(
        &self,
        platform: &str,
        capability: Capability,
        now: i64,
    ) -> MonitorResult<()> {
        self.store
            .set_capability_override(&CapabilityOverride {
                platform: platform.to_owned(),
                capability: capability.as_str().to_owned(),
                disabled: false,
                reason: String::new(),
                changed_at: now,
            })
            .await?;
        Ok(())
    }

    /// Whether the switch currently refuses (platform, capability).
    /// Absent rows mean no operator decision — allowed.
    pub async fn denies(&self, platform: &str, capability: Capability) -> MonitorResult<bool> {
        Ok(self
            .store
            .find_capability_override(platform, capability.as_str())
            .await?
            .is_some_and(|row| row.disabled))
    }

    /// Every override row, ordered by platform then capability.
    pub async fn overrides(&self) -> MonitorResult<Vec<CapabilityOverride>> {
        Ok(self.store.list_capability_overrides().await?)
    }
}
