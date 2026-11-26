//! F028 acceptance (rollout audit side): fleet mutations become
//! incidents in the platform-wide scope — promotes as info, the
//! rollback as a warning, replays as no-ops.

#[allow(dead_code)]
mod common;

use std::sync::Arc;

use async_trait::async_trait;
use common::{gated, unique_token};
use fishpond_domain::{Conversation, ConversationId, Message, Product};
use fishpond_monitor::{IncidentLog, RolloutAuditor};
use fishpond_provider_sdk::{
    AuthorizeOutcome, CallContext, Capability, EngineKind, ProtocolVersion, Provider,
    ProviderError, ProviderManifest, ProviderResult, SendMessageOutcome, SendMessageRequest,
};
use fishpond_router::{CanaryPolicy, VersionedFleet};

const SECRET: &str = "whsec-f028-rollout-test";

struct StubProvider {
    manifest: ProviderManifest,
}

fn manifest_of(platform: &str, version: &str) -> ProviderManifest {
    ProviderManifest {
        provider_id: platform.to_owned(),
        display_name: "rollout stub".to_owned(),
        provider_version: version.to_owned(),
        engine: EngineKind::Api,
        protocol_versions: vec![ProtocolVersion::V1_0],
        capabilities: vec![Capability::Authorize],
    }
}

#[async_trait]
impl Provider for StubProvider {
    fn manifest(&self) -> &ProviderManifest {
        &self.manifest
    }

    async fn authorize(&self, _ctx: &CallContext) -> ProviderResult<AuthorizeOutcome> {
        Err(ProviderError::new(
            fishpond_provider_sdk::ErrorClass::Unsupported,
            "stub",
            "stub serves nothing",
        ))
    }

    async fn send_message(
        &self,
        _ctx: &CallContext,
        _request: SendMessageRequest,
    ) -> ProviderResult<SendMessageOutcome> {
        Err(ProviderError::new(
            fishpond_provider_sdk::ErrorClass::Unsupported,
            "stub",
            "stub serves nothing",
        ))
    }

    async fn list_conversations(&self, _ctx: &CallContext) -> ProviderResult<Vec<Conversation>> {
        Err(ProviderError::new(
            fishpond_provider_sdk::ErrorClass::Unsupported,
            "stub",
            "stub serves nothing",
        ))
    }

    async fn list_messages(
        &self,
        _ctx: &CallContext,
        _conversation: &ConversationId,
    ) -> ProviderResult<Vec<Message>> {
        Err(ProviderError::new(
            fishpond_provider_sdk::ErrorClass::Unsupported,
            "stub",
            "stub serves nothing",
        ))
    }

    async fn list_products(&self, _ctx: &CallContext) -> ProviderResult<Vec<Product>> {
        Err(ProviderError::new(
            fishpond_provider_sdk::ErrorClass::Unsupported,
            "stub",
            "stub serves nothing",
        ))
    }
}

#[tokio::test]
async fn fleet_moves_leave_an_incident_trail() {
    let Some(store) = gated().await else {
        eprintln!("skip: no database");
        return;
    };
    let store = Arc::new(store);
    let log = IncidentLog::new(store.clone());
    let auditor = RolloutAuditor::new(IncidentLog::new(store));
    let platform = unique_token("f028-roll");

    // Two coexisting versions, one promote, one canary, one rollback.
    let fleet = VersionedFleet::new();
    for version in ["1.0", "1.1"] {
        let manifest = manifest_of(&platform, version);
        let package =
            fishpond_provider_sdk::ProviderPackage::sign(manifest.clone(), SECRET).expect("signs");
        fleet
            .register(Arc::new(StubProvider { manifest }), &package, SECRET)
            .expect("registers");
    }
    fleet.promote(&platform, "1.1").expect("promotes");
    fleet
        .start_canary(
            &platform,
            CanaryPolicy {
                version: "1.0".to_owned(),
                percent: 20,
            },
        )
        .expect("canary starts");
    fleet.stop_canary(&platform).expect("canary stops");
    fleet.rollback(&platform).expect("rolls back");

    let changes = fleet.take_changes();
    assert_eq!(changes.len(), 4, "promote + canary start/stop + rollback");
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock is after the epoch")
        .as_millis() as i64;
    assert_eq!(
        auditor.record_all(&changes, now).await.expect("record"),
        4,
        "all four are new"
    );

    // The rollback incident is a warning; the rest are info.
    let rollback = &changes[3];
    let id = format!(
        "rollout-{}-{platform}-from-{}-to-{}",
        rollback.action.as_str(),
        rollback.from.as_deref().unwrap_or("-"),
        rollback.to.as_deref().unwrap_or("-"),
    );
    let incident = log
        .get("", &id)
        .await
        .expect("get")
        .expect("rollback incident stands");
    assert_eq!(incident.severity, "warning", "retreating is a warning");
    assert_eq!(incident.kind, "rollout");
    assert_eq!(incident.tenant, "", "rollouts are platform-wide");

    // Re-recording the same trail replays as no-ops.
    assert_eq!(auditor.record_all(&changes, now).await.expect("replay"), 0);
}
