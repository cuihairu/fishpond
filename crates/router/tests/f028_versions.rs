//! F028 acceptance (fleet side): signed registration with digest
//! consistency, version coexistence, promote/rollback, the
//! deterministic canary lane, and the operator trail. Pure — no
//! database.

use std::sync::Arc;

use async_trait::async_trait;
use fishpond_domain::{Conversation, ConversationId, Message, Product};
use fishpond_provider_sdk::{
    AuthorizeOutcome, CallContext, Capability, EngineKind, ErrorClass, PackageError,
    ProtocolVersion, Provider, ProviderError, ProviderManifest, ProviderPackage, ProviderResult,
    SendMessageOutcome, SendMessageRequest,
};
use fishpond_router::{CanaryPolicy, FleetAction, FleetChange, FleetError, VersionedFleet};

const SECRET: &str = "whsec-f028-test";

/// A versioned provider stub: manifest is configurable, everything it
/// serves is Unsupported (the fleet tests never execute a call).
struct StubProvider {
    manifest: ProviderManifest,
}

fn manifest_of(version: &str, caps: &[Capability]) -> ProviderManifest {
    ProviderManifest {
        provider_id: "stub".to_owned(),
        display_name: "versioned stub".to_owned(),
        provider_version: version.to_owned(),
        engine: EngineKind::Api,
        protocol_versions: vec![ProtocolVersion::V1_0],
        capabilities: caps.to_vec(),
    }
}

fn unsupported() -> ProviderError {
    ProviderError::new(ErrorClass::Unsupported, "stub", "stub serves nothing")
}

#[async_trait]
impl Provider for StubProvider {
    fn manifest(&self) -> &ProviderManifest {
        &self.manifest
    }

    async fn authorize(&self, _ctx: &CallContext) -> ProviderResult<AuthorizeOutcome> {
        Err(unsupported())
    }

    async fn send_message(
        &self,
        _ctx: &CallContext,
        _request: SendMessageRequest,
    ) -> ProviderResult<SendMessageOutcome> {
        Err(unsupported())
    }

    async fn list_conversations(&self, _ctx: &CallContext) -> ProviderResult<Vec<Conversation>> {
        Err(unsupported())
    }

    async fn list_messages(
        &self,
        _ctx: &CallContext,
        _conversation: &ConversationId,
    ) -> ProviderResult<Vec<Message>> {
        Err(unsupported())
    }

    async fn list_products(&self, _ctx: &CallContext) -> ProviderResult<Vec<Product>> {
        Err(unsupported())
    }
}

fn stub(version: &str, caps: &[Capability]) -> Arc<dyn Provider> {
    Arc::new(StubProvider {
        manifest: manifest_of(version, caps),
    })
}

fn package(version: &str, caps: &[Capability]) -> ProviderPackage {
    ProviderPackage::sign(manifest_of(version, caps), SECRET).expect("package signs")
}

/// Registers v1.0 (Authorize only) and v1.1 (Authorize + SendMessage)
/// as coexisting versions of the same platform.
fn fleet_with_two_versions() -> VersionedFleet {
    let fleet = VersionedFleet::new();
    fleet
        .register(
            stub("1.0", &[Capability::Authorize]),
            &package("1.0", &[Capability::Authorize]),
            SECRET,
        )
        .expect("v1.0 registers");
    fleet
        .register(
            stub("1.1", &[Capability::Authorize, Capability::SendMessage]),
            &package("1.1", &[Capability::Authorize, Capability::SendMessage]),
            SECRET,
        )
        .expect("v1.1 registers");
    fleet
}

#[test]
fn tampered_packages_never_register() {
    let fleet = VersionedFleet::new();
    let mut tampered = package("1.0", &[Capability::Authorize]);
    tampered.manifest.provider_version = "9.9".to_owned();
    assert_eq!(
        fleet.register(stub("1.0", &[Capability::Authorize]), &tampered, SECRET),
        Err(FleetError::Package(PackageError::BadSignature))
    );
    assert!(
        fleet.view("stub").is_none(),
        "a rejected package creates no fleet"
    );
}

#[test]
fn registration_is_verified_idempotent_and_describe_checked() {
    let fleet = VersionedFleet::new();
    let provider = stub("1.0", &[Capability::Authorize]);

    assert!(
        fleet
            .register(
                provider.clone(),
                &package("1.0", &[Capability::Authorize]),
                SECRET
            )
            .expect("first registration is new")
    );
    assert!(
        !fleet
            .register(
                provider.clone(),
                &package("1.0", &[Capability::Authorize]),
                SECRET
            )
            .expect("same content replays idempotently")
    );

    // Same version, different content — silent drift is refused.
    assert_eq!(
        fleet.register(
            stub("1.0", &[Capability::Authorize, Capability::SendMessage]),
            &package("1.0", &[Capability::Authorize, Capability::SendMessage]),
            SECRET,
        ),
        Err(FleetError::DigestConflict {
            platform: "stub".to_owned(),
            version: "1.0".to_owned(),
        })
    );

    // The package must describe the instance it ships with.
    assert_eq!(
        fleet.register(
            stub("2.0", &[Capability::SendMessage]),
            &package("2.0", &[Capability::Authorize]),
            SECRET
        ),
        Err(FleetError::ManifestMismatch {
            provider_id: "stub".to_owned(),
            version: "2.0".to_owned(),
        })
    );

    let view = fleet.view("stub").expect("fleet exists");
    assert_eq!(view.versions, vec!["1.0".to_owned()]);
    assert_eq!(view.active, Some("1.0".to_owned()));
    assert_eq!(view.previous, None);
}

#[test]
fn promote_and_rollback_switch_the_lane() {
    let fleet = fleet_with_two_versions();
    let served = |fleet: &VersionedFleet| {
        fleet
            .provider_for("stub", "any-key")
            .expect("a lane serves")
            .manifest()
            .provider_version
            .clone()
    };

    assert_eq!(
        served(&fleet),
        "1.0",
        "the first registered version is active"
    );

    assert_eq!(
        fleet.promote("stub", "2.0"),
        Err(FleetError::UnknownVersion {
            platform: "stub".to_owned(),
            version: "2.0".to_owned(),
        })
    );
    assert_eq!(
        fleet.promote("stub", "1.0"),
        Err(FleetError::AlreadyActive {
            platform: "stub".to_owned(),
            version: "1.0".to_owned(),
        })
    );

    fleet.promote("stub", "1.1").expect("promotes");
    assert_eq!(served(&fleet), "1.1");
    let view = fleet.view("stub").expect("fleet exists");
    assert_eq!(view.previous, Some("1.0".to_owned()));

    fleet.rollback("stub").expect("rolls back");
    assert_eq!(served(&fleet), "1.0");
    assert_eq!(
        fleet.rollback("stub"),
        Err(FleetError::NothingToRollback {
            platform: "stub".to_owned(),
        }),
        "the rewind target was consumed"
    );
    assert_eq!(
        fleet.promote("ghost", "1.0"),
        Err(FleetError::UnknownPlatform {
            platform: "ghost".to_owned(),
        })
    );
}

#[test]
fn canary_is_deterministic_per_routing_key() {
    let fleet = fleet_with_two_versions();

    assert_eq!(
        fleet.start_canary(
            "stub",
            CanaryPolicy {
                version: "1.0".to_owned(),
                percent: 50
            }
        ),
        Err(FleetError::CanaryNeedsOtherVersion {
            platform: "stub".to_owned(),
            version: "1.0".to_owned(),
        })
    );
    assert_eq!(
        fleet.start_canary(
            "stub",
            CanaryPolicy {
                version: "1.1".to_owned(),
                percent: 0
            }
        ),
        Err(FleetError::BadPercent(0))
    );
    assert_eq!(
        fleet.start_canary(
            "stub",
            CanaryPolicy {
                version: "1.1".to_owned(),
                percent: 100
            }
        ),
        Err(FleetError::BadPercent(100))
    );
    assert_eq!(
        fleet.start_canary(
            "stub",
            CanaryPolicy {
                version: "9.9".to_owned(),
                percent: 50
            }
        ),
        Err(FleetError::UnknownVersion {
            platform: "stub".to_owned(),
            version: "9.9".to_owned(),
        })
    );

    fleet
        .start_canary(
            "stub",
            CanaryPolicy {
                version: "1.1".to_owned(),
                percent: 50,
            },
        )
        .expect("canary starts");

    let keys: Vec<String> = (0..40).map(|i| format!("tenant-{i}")).collect();
    let lane_of = |fleet: &VersionedFleet, key: &str| {
        fleet
            .provider_for("stub", key)
            .expect("a lane serves")
            .manifest()
            .provider_version
            .clone()
    };

    // Deterministic: the same key always lands in the same lane.
    for key in &keys {
        assert_eq!(
            lane_of(&fleet, key),
            lane_of(&fleet, key),
            "{key} flipped lanes"
        );
    }
    // Both lanes carry traffic, and roughly the promised share does.
    let canary_keys = keys.iter().filter(|k| lane_of(&fleet, k) == "1.1").count();
    assert!(canary_keys > 0, "some keys take the canary lane");
    assert!(
        canary_keys < keys.len(),
        "some keys stay on the active lane"
    );
    assert!(
        (5..=35).contains(&canary_keys),
        "50% canary carried {canary_keys} of {} keys",
        keys.len()
    );

    // Stop returns every key to the active lane.
    fleet.stop_canary("stub").expect("canary stops");
    for key in &keys {
        assert_eq!(
            lane_of(&fleet, key),
            "1.0",
            "{key} stayed off the active lane after stop"
        );
    }
    assert_eq!(
        fleet.stop_canary("stub"),
        Ok(()),
        "stopping twice is a no-op"
    );
}

#[test]
fn rollback_closes_the_canary_and_the_trail_records_every_move() {
    let fleet = fleet_with_two_versions();
    fleet.promote("stub", "1.1").expect("promotes");
    fleet
        .start_canary(
            "stub",
            CanaryPolicy {
                version: "1.0".to_owned(),
                percent: 10,
            },
        )
        .expect("canary starts on the old version");
    fleet.rollback("stub").expect("rolls back");
    let view = fleet.view("stub").expect("fleet exists");
    assert_eq!(view.canary, None, "rewinding closes the experiment");
    assert_eq!(view.active, Some("1.0".to_owned()));

    let changes = fleet.take_changes();
    assert_eq!(
        changes,
        vec![
            FleetChange {
                action: FleetAction::Promote,
                platform: "stub".to_owned(),
                from: Some("1.0".to_owned()),
                to: Some("1.1".to_owned()),
            },
            FleetChange {
                action: FleetAction::CanaryStart,
                platform: "stub".to_owned(),
                from: Some("1.1".to_owned()),
                to: Some("1.0".to_owned()),
            },
            FleetChange {
                action: FleetAction::Rollback,
                platform: "stub".to_owned(),
                from: Some("1.1".to_owned()),
                to: Some("1.0".to_owned()),
            },
        ]
    );
    assert!(fleet.take_changes().is_empty(), "the trail drains");
}
