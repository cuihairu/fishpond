//! F028 versioned provider fleet: coexisting manifest-verified provider
//! versions per platform, operator promotion and rollback, and a
//! deterministic canary lane for gradual rollout.
//!
//! Registration only accepts [`ProviderPackage`]s whose HMAC signature
//! verifies and whose manifest actually describes the provider instance
//! handed in — a lying or tampered manifest never enters the table.
//! The fleet is the registration face; scope checks and the breaker
//! still live in [`CapabilityRouter`].

use std::collections::{BTreeMap, HashMap};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::{Arc, Mutex};

use fishpond_provider_sdk::{PackageError, Provider, ProviderPackage};

/// Why a fleet operation was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FleetError {
    /// The package failed signature or manifest validation.
    Package(PackageError),
    /// No fleet exists for the platform.
    UnknownPlatform {
        /// The platform looked up.
        platform: String,
    },
    /// The named version was never registered.
    UnknownVersion {
        /// The platform looked up.
        platform: String,
        /// The version looked up.
        version: String,
    },
    /// The version is already registered with different content —
    /// silent manifest drift is never overwritten.
    DigestConflict {
        /// The platform holding the conflicting version.
        platform: String,
        /// The conflicting version.
        version: String,
    },
    /// The package's manifest does not describe the provider instance
    /// it ships with (content digest mismatch).
    ManifestMismatch {
        /// The provider the package claims to describe.
        provider_id: String,
        /// The version the package claims.
        version: String,
    },
    /// The version is already the active one.
    AlreadyActive {
        /// The platform.
        platform: String,
        /// The already-active version.
        version: String,
    },
    /// Rollback was asked for but no previous active version exists.
    NothingToRollback {
        /// The platform.
        platform: String,
    },
    /// The canary version must differ from the active one.
    CanaryNeedsOtherVersion {
        /// The platform.
        platform: String,
        /// The canary version that equals the active one.
        version: String,
    },
    /// The canary percent is outside `1..=99` (0 and 100 are the
    /// no-op/promote domains, not gray release).
    BadPercent(u8),
}

impl std::fmt::Display for FleetError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Package(e) => write!(f, "provider package rejected: {e}"),
            Self::UnknownPlatform { platform } => write!(f, "no fleet for platform {platform:?}"),
            Self::UnknownVersion { platform, version } => {
                write!(f, "platform {platform:?} has no version {version:?}")
            }
            Self::DigestConflict { platform, version } => write!(
                f,
                "platform {platform:?} version {version:?} already registered with different content"
            ),
            Self::ManifestMismatch {
                provider_id,
                version,
            } => write!(
                f,
                "package manifest does not describe provider {provider_id:?} version {version:?}"
            ),
            Self::AlreadyActive { platform, version } => {
                write!(
                    f,
                    "platform {platform:?} version {version:?} is already active"
                )
            }
            Self::NothingToRollback { platform } => {
                write!(
                    f,
                    "platform {platform:?} has no previous version to roll back to"
                )
            }
            Self::CanaryNeedsOtherVersion { platform, version } => write!(
                f,
                "canary version {version:?} must differ from the active one on {platform:?}"
            ),
            Self::BadPercent(percent) => write!(f, "canary percent {percent} must be in 1..=99"),
        }
    }
}

impl std::error::Error for FleetError {}

impl From<PackageError> for FleetError {
    fn from(e: PackageError) -> Self {
        Self::Package(e)
    }
}

/// The gray-release policy: route `percent`% of routing keys to the
/// candidate version, deterministically per key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CanaryPolicy {
    /// The candidate version (registered, different from active).
    pub version: String,
    /// Share of routing keys sent to the candidate, `1..=99`.
    pub percent: u8,
}

/// One recorded fleet mutation, for the operator trail (the monitor
/// turns these into incidents).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FleetAction {
    /// A version became active.
    Promote,
    /// The previous active version was restored.
    Rollback,
    /// A canary lane started.
    CanaryStart,
    /// A canary lane stopped.
    CanaryStop,
}

impl FleetAction {
    /// Canonical wire name.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Promote => "promote",
            Self::Rollback => "rollback",
            Self::CanaryStart => "canary-start",
            Self::CanaryStop => "canary-stop",
        }
    }
}

/// One fleet mutation: `from`/`to` carry the active version before and
/// after (for canary records, `to` is the candidate version).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FleetChange {
    /// What happened.
    pub action: FleetAction,
    /// The platform the mutation hit.
    pub platform: String,
    /// The active version before the mutation.
    pub from: Option<String>,
    /// The active version after (canary records: the candidate).
    pub to: Option<String>,
}

/// One registered version of a platform's provider.
#[derive(Clone)]
struct VersionSlot {
    provider: Arc<dyn Provider>,
    digest: u64,
}

/// Everything registered for one platform.
#[derive(Default)]
struct PlatformFleet {
    versions: BTreeMap<String, VersionSlot>,
    active: Option<String>,
    previous: Option<String>,
    canary: Option<CanaryPolicy>,
}

/// Read-only view of one platform's fleet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FleetView {
    /// The version new traffic settles on (canary aside).
    pub active: Option<String>,
    /// The version rollback would restore.
    pub previous: Option<String>,
    /// The gray-release policy, if one is running.
    pub canary: Option<CanaryPolicy>,
    /// Every registered version.
    pub versions: Vec<String>,
}

#[derive(Default)]
struct FleetState {
    fleets: HashMap<String, PlatformFleet>,
    trail: Vec<FleetChange>,
}

/// The versioned registration face over all platforms.
#[derive(Default)]
pub struct VersionedFleet {
    state: Mutex<FleetState>,
}

impl VersionedFleet {
    /// Builds an empty fleet.
    pub fn new() -> Self {
        Self {
            state: Mutex::new(FleetState::default()),
        }
    }

    /// Verifies `package` under `secret`, checks its manifest actually
    /// describes `provider`, and registers it under the manifest's
    /// provider version. Re-registering the same version with identical
    /// content is idempotent (returns false); a different content under
    /// the same version is a [`FleetError::DigestConflict`].
    pub fn register(
        &self,
        provider: Arc<dyn Provider>,
        package: &ProviderPackage,
        secret: &str,
    ) -> Result<bool, FleetError> {
        package.verify(secret)?;
        let manifest = &package.manifest;
        let platform = manifest.provider_id.clone();
        let version = manifest.provider_version.clone();
        let digest = manifest.content_digest();
        if provider.manifest().content_digest() != digest {
            return Err(FleetError::ManifestMismatch {
                provider_id: platform.clone(),
                version: version.clone(),
            });
        }

        let mut state = self.state.lock().expect("fleet lock");
        let fleet = state.fleets.entry(platform).or_default();
        if let Some(slot) = fleet.versions.get(&version) {
            if slot.digest == digest {
                return Ok(false);
            }
            return Err(FleetError::DigestConflict {
                platform: package.manifest.provider_id.clone(),
                version,
            });
        }
        fleet
            .versions
            .insert(version.clone(), VersionSlot { provider, digest });
        if fleet.active.is_none() {
            fleet.active = Some(version.clone());
        }
        Ok(true)
    }

    /// Makes `version` the active one; the old active becomes the
    /// rollback target.
    pub fn promote(&self, platform: &str, version: &str) -> Result<(), FleetError> {
        self.mutate(platform, |fleet| {
            if !fleet.versions.contains_key(version) {
                return Err(FleetError::UnknownVersion {
                    platform: platform.to_owned(),
                    version: version.to_owned(),
                });
            }
            if fleet.active.as_deref() == Some(version) {
                return Err(FleetError::AlreadyActive {
                    platform: platform.to_owned(),
                    version: version.to_owned(),
                });
            }
            let from = fleet.active.replace(version.to_owned());
            fleet.previous = from.clone();
            Ok(Some((FleetAction::Promote, from, Some(version.to_owned()))))
        })
    }

    /// Restores the previous active version. The rewind target is
    /// consumed — a second rollback in a row has nothing to restore —
    /// and a running canary lane rides along: rewinding the active
    /// version invalidates the experiment.
    pub fn rollback(&self, platform: &str) -> Result<(), FleetError> {
        self.mutate(platform, |fleet| {
            let Some(previous) = fleet.previous.clone() else {
                return Err(FleetError::NothingToRollback {
                    platform: platform.to_owned(),
                });
            };
            let from = fleet.active.replace(previous.clone());
            fleet.previous = None;
            fleet.canary = None;
            Ok(Some((FleetAction::Rollback, from, Some(previous))))
        })
    }

    /// Starts routing `percent`% of routing keys to the candidate.
    pub fn start_canary(&self, platform: &str, policy: CanaryPolicy) -> Result<(), FleetError> {
        if !(1..=99).contains(&policy.percent) {
            return Err(FleetError::BadPercent(policy.percent));
        }
        self.mutate(platform, |fleet| {
            if !fleet.versions.contains_key(&policy.version) {
                return Err(FleetError::UnknownVersion {
                    platform: platform.to_owned(),
                    version: policy.version.clone(),
                });
            }
            if fleet.active.as_deref() == Some(policy.version.as_str()) {
                return Err(FleetError::CanaryNeedsOtherVersion {
                    platform: platform.to_owned(),
                    version: policy.version.clone(),
                });
            }
            fleet.canary = Some(policy.clone());
            Ok(Some((
                FleetAction::CanaryStart,
                fleet.active.clone(),
                Some(policy.version),
            )))
        })
    }

    /// Stops the canary lane; all routing keys return to the active
    /// version.
    pub fn stop_canary(&self, platform: &str) -> Result<(), FleetError> {
        self.mutate(platform, |fleet| {
            let to = fleet.canary.take().map(|policy| policy.version);
            Ok(to.map(|version| (FleetAction::CanaryStop, None, Some(version))))
        })
    }

    /// The provider serving `route_key` on `platform`: the active
    /// version, except that a running canary sends a deterministic
    /// share of keys to the candidate (the same key always lands in the
    /// same lane, across calls and restarts).
    pub fn provider_for(&self, platform: &str, route_key: &str) -> Option<Arc<dyn Provider>> {
        let state = self.state.lock().expect("fleet lock");
        let fleet = state.fleets.get(platform)?;
        let active = fleet.active.as_ref()?;
        let lane = match &fleet.canary {
            Some(policy) if bucket(route_key) < u64::from(policy.percent) => &policy.version,
            _ => active,
        };
        Some(
            fleet
                .versions
                .get(lane)
                .or_else(|| fleet.versions.get(active))?
                .provider
                .clone(),
        )
    }

    /// Read-only view of one platform's fleet.
    pub fn view(&self, platform: &str) -> Option<FleetView> {
        let state = self.state.lock().expect("fleet lock");
        let fleet = state.fleets.get(platform)?;
        Some(FleetView {
            active: fleet.active.clone(),
            previous: fleet.previous.clone(),
            canary: fleet.canary.clone(),
            versions: fleet.versions.keys().cloned().collect(),
        })
    }

    /// Drains the operator trail of every mutation so far.
    pub fn take_changes(&self) -> Vec<FleetChange> {
        let mut state = self.state.lock().expect("fleet lock");
        std::mem::take(&mut state.trail)
    }

    /// Runs `op` against one platform's fleet and records the change it
    /// reports; the platform must already exist (registration creates
    /// fleets, mutations never do).
    fn mutate(
        &self,
        platform: &str,
        op: impl FnOnce(
            &mut PlatformFleet,
        )
            -> Result<Option<(FleetAction, Option<String>, Option<String>)>, FleetError>,
    ) -> Result<(), FleetError> {
        let mut state = self.state.lock().expect("fleet lock");
        let Some(fleet) = state.fleets.get_mut(platform) else {
            return Err(FleetError::UnknownPlatform {
                platform: platform.to_owned(),
            });
        };
        if let Some((action, from, to)) = op(fleet)? {
            state.trail.push(FleetChange {
                action,
                platform: platform.to_owned(),
                from,
                to,
            });
        }
        Ok(())
    }
}

/// A stable bucket `0..=99` for one routing key — deterministic across
/// processes (`DefaultHasher::new` fixes its keys).
fn bucket(route_key: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    route_key.hash(&mut hasher);
    hasher.finish() % 100
}
