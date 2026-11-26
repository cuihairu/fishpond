//! F034 service-face acceptance: the takeover service binds short-lived
//! operator credentials to the F011 coordination state machine. Issue
//! flips an AUTHORIZED session to TAKEN_OVER and mints exactly one
//! token; verification is pure possession; release revokes the grant
//! and hands the session back to automation. Skipped when
//! `FISHPOND_TEST_DATABASE_URL` is unset.

mod common;

use common::{ENV, account, gated, tenant_record, ts, unique_entity, unique_tenant};
use fishpond_domain::{AccountId, AuthSessionId, AuthSessionState, TenantId, UnixMillis};
use fishpond_server::{ServiceError, SessionCoordinator, Sessions, TakeoverService};
use fishpond_storage::{CredentialCipher, MasterKey, NewCredential, SecretBox, Store};

const SECRET_V1: &str = "f034-secret-generation-one";

/// Everything one test needs: tenant, account, credential, an
/// AUTHORIZED session, and the services on top.
struct World {
    tenant: TenantId,
    account: AccountId,
    credential: String,
    session: AuthSessionId,
    store: Store,
    coordinator: SessionCoordinator,
    takeover: TakeoverService,
}

impl World {
    /// Builds a fresh world; `None` when CI has no database.
    async fn new(tag: &str) -> Option<World> {
        let store = gated().await?;
        let tenant = unique_tenant(tag);
        store
            .upsert_tenant(&tenant_record(&tenant, tag))
            .await
            .expect("tenant upsert");
        let account_id = AccountId::parse(unique_entity("acc", tag)).expect("account id");
        store
            .upsert_account(&account(
                account_id.as_str(),
                &tenant,
                fishpond_domain::AccountState::Online,
            ))
            .await
            .expect("account upsert");
        let credential = unique_entity("cred", tag);
        let cipher = CredentialCipher::new(MasterKey::generate(), 1);
        store
            .store_credential(NewCredential {
                id: &credential,
                tenant: &tenant,
                platform: "mock",
                kind: "api_token",
                cipher: &cipher,
                secret: SecretBox::new(SECRET_V1),
                created_at: ts(900),
            })
            .await
            .expect("credential store");

        let sessions = Sessions::new(store.clone());
        let coordinator = SessionCoordinator::new(sessions.clone());
        let session = AuthSessionId::parse(unique_entity("sess", tag)).expect("session id");
        sessions
            .start(
                &tenant,
                session.clone(),
                account_id.clone(),
                ts(1_000),
                None,
            )
            .await
            .expect("session start");
        let version = sessions
            .credential_version(&tenant, &credential)
            .await
            .expect("credential version");
        coordinator
            .authorize_exclusive(&tenant, &session, &credential, version, None, ts(2_000))
            .await
            .expect("exclusive authorize");

        Some(World {
            takeover: TakeoverService::new(
                store.clone(),
                SessionCoordinator::new(sessions.clone()),
            ),
            tenant,
            account: account_id,
            credential,
            session,
            store,
            coordinator,
        })
    }

    /// Issues a 60s grant through the service under test.
    async fn issue(&self) -> (String, String, UnixMillis) {
        self.takeover
            .issue(&self.tenant, &self.session, "op-f034", 60_000, ts(3_000))
            .await
            .expect("grant issue")
    }

    /// The session's current state.
    async fn state(&self) -> AuthSessionState {
        self.coordinator
            .get(&self.tenant, &self.session)
            .await
            .expect("session get")
            .state
    }
}

#[tokio::test]
async fn issue_flips_to_taken_over_and_the_token_verifies() {
    let Some(world) = World::new("f034-issue").await else {
        eprintln!("skip: {ENV} is not set (CI has no database)");
        return;
    };
    let (grant, token, expires_at) = world.issue().await;

    // The state machine moved to the operator's hands.
    assert_eq!(world.state().await, AuthSessionState::TakenOver);
    // The token looks like a takeover credential and carries a life.
    assert!(token.starts_with("fp_tk_"), "{token}");
    assert_eq!(expires_at, ts(63_000));
    assert!(!grant.is_empty());

    // Verification is pure possession: it resolves the whole context.
    let seen = world
        .takeover
        .verify(&token, ts(4_000))
        .await
        .expect("verify");
    assert_eq!(seen.tenant, world.tenant);
    assert_eq!(seen.account, world.account);
    assert_eq!(seen.session, world.session);
    assert_eq!(seen.operator, "op-f034");
    assert_eq!(seen.expires_at, expires_at);
}

#[tokio::test]
async fn issue_requires_an_authorized_session() {
    let Some(world) = World::new("f034-pending").await else {
        eprintln!("skip: {ENV} is not set (CI has no database)");
        return;
    };
    // A fresh PENDING session cannot hand out operator credentials.
    let pending = AuthSessionId::parse(unique_entity("sess", "pending")).expect("session id");
    world
        .store
        .clone()
        .upsert_tenant(&tenant_record(&world.tenant, "f034-pending"))
        .await
        .expect("tenant upsert (idempotent)");
    let sessions = Sessions::new(world.store.clone());
    sessions
        .start(
            &world.tenant,
            pending.clone(),
            world.account.clone(),
            ts(1_000),
            None,
        )
        .await
        .expect("pending session start");
    let refused = world
        .takeover
        .issue(&world.tenant, &pending, "op-f034", 60_000, ts(3_000))
        .await;
    assert!(
        matches!(
            refused,
            Err(ServiceError::Conflict { .. }) | Err(ServiceError::Domain(_))
        ),
        "PENDING must refuse takeover, got {refused:?}"
    );
    assert_eq!(world.state_of(&pending).await, AuthSessionState::Pending);
}

#[tokio::test]
async fn lifetimes_are_short_and_capped() {
    let Some(world) = World::new("f034-ttl").await else {
        eprintln!("skip: {ENV} is not set (CI has no database)");
        return;
    };
    for bad in [0i64, -1_000, 900_001] {
        let refused = world
            .takeover
            .issue(&world.tenant, &world.session, "op-f034", bad, ts(3_000))
            .await;
        assert!(
            matches!(refused, Err(ServiceError::Invalid { .. })),
            "ttl {bad} must be refused, got {refused:?}"
        );
    }
    // The cap itself is allowed.
    world
        .takeover
        .issue(&world.tenant, &world.session, "op-f034", 900_000, ts(3_000))
        .await
        .expect("ttl at the cap issues");
}

#[tokio::test]
async fn verification_dies_on_expiry_revocation_and_strangers() {
    let Some(world) = World::new("f034-verify").await else {
        eprintln!("skip: {ENV} is not set (CI has no database)");
        return;
    };
    let stranger = fishpond_storage::generate_takeover_token();
    assert!(
        matches!(
            world.takeover.verify(&stranger, ts(4_000)).await,
            Err(ServiceError::NotFound { .. })
        ),
        "an unissued token must verify to nothing"
    );

    // A minimal-life grant on a second session proves expiry. The
    // second session needs its own account: F011 lets an account hold
    // exactly one live session.
    let second_account = AccountId::parse(unique_entity("acc", "verify-2")).expect("account id");
    world
        .store
        .upsert_account(&account(
            second_account.as_str(),
            &world.tenant,
            fishpond_domain::AccountState::Online,
        ))
        .await
        .expect("second account upsert");
    let second = AuthSessionId::parse(unique_entity("sess", "verify-2")).expect("session id");
    let sessions = Sessions::new(world.store.clone());
    sessions
        .start(
            &world.tenant,
            second.clone(),
            second_account.clone(),
            ts(1_000),
            None,
        )
        .await
        .expect("second session start");
    let version = sessions
        .credential_version(&world.tenant, &world.credential)
        .await
        .expect("credential version");
    world
        .coordinator
        .authorize_exclusive(
            &world.tenant,
            &second,
            &world.credential,
            version,
            None,
            ts(2_000),
        )
        .await
        .expect("authorize second");
    let (_, short_token, _) = world
        .takeover
        .issue(&world.tenant, &second, "op-f034", 1_000, ts(3_000))
        .await
        .expect("short grant");
    assert!(
        matches!(
            world.takeover.verify(&short_token, ts(4_000)).await,
            Err(ServiceError::NotFound { .. })
        ),
        "at ts(4_000) the 1s grant (3_000→4_000) is already expired"
    );

    // Release revokes: the first session's token stops verifying too.
    let (grant, first_token, _) = world.issue().await;
    let released = world
        .takeover
        .release(&world.tenant, &world.session, &grant, ts(5_000))
        .await
        .expect("release");
    assert!(released, "the live grant must release");
    assert!(
        matches!(
            world.takeover.verify(&first_token, ts(6_000)).await,
            Err(ServiceError::NotFound { .. })
        ),
        "a released token must verify to nothing"
    );
}

#[tokio::test]
async fn release_revokes_and_hands_back_to_automation() {
    let Some(world) = World::new("f034-release").await else {
        eprintln!("skip: {ENV} is not set (CI has no database)");
        return;
    };
    let (grant, token, _) = world.issue().await;
    assert_eq!(world.state().await, AuthSessionState::TakenOver);

    let released = world
        .takeover
        .release(&world.tenant, &world.session, &grant, ts(4_000))
        .await
        .expect("release");
    assert!(released);
    assert_eq!(world.state().await, AuthSessionState::Authorized);
    assert!(
        matches!(
            world.takeover.verify(&token, ts(5_000)).await,
            Err(ServiceError::NotFound { .. })
        ),
        "the released token must be dead"
    );

    // A second release is a no-op, not an error.
    let again = world
        .takeover
        .release(&world.tenant, &world.session, &grant, ts(6_000))
        .await
        .expect("second release");
    assert!(!again);
}

#[tokio::test]
async fn a_live_grant_blocks_a_second_issue_until_release() {
    let Some(world) = World::new("f034-single").await else {
        eprintln!("skip: {ENV} is not set (CI has no database)");
        return;
    };
    let (first, _, _) = world.issue().await;
    let refused = world
        .takeover
        .issue(&world.tenant, &world.session, "op-evil", 60_000, ts(4_000))
        .await;
    assert!(
        matches!(refused, Err(ServiceError::Conflict { .. })),
        "a live grant must block re-issue, got {refused:?}"
    );

    // After release the same session can hand out a fresh credential.
    world
        .takeover
        .release(&world.tenant, &world.session, &first, ts(5_000))
        .await
        .expect("release");
    let (second, second_token, _) = world.issue().await;
    assert_ne!(second, first, "a re-issue must mint a new grant id");
    assert!(
        world
            .takeover
            .verify(&second_token, ts(6_000))
            .await
            .is_ok(),
        "the fresh credential verifies"
    );
}

#[tokio::test]
async fn a_foreign_tenant_cannot_release_a_takeover() {
    let Some(world) = World::new("f034-foreign").await else {
        eprintln!("skip: {ENV} is not set (CI has no database)");
        return;
    };
    let stranger = unique_tenant("f034-foreign-stranger");
    world
        .store
        .upsert_tenant(&tenant_record(&stranger, "f034-foreign-stranger"))
        .await
        .expect("stranger tenant upsert");

    let (grant, token, _) = world.issue().await;
    let released = world
        .takeover
        .release(&stranger, &world.session, &grant, ts(4_000))
        .await
        .expect("foreign release is a no-op");
    assert!(!released, "a foreign tenant must not release");
    assert_eq!(world.state().await, AuthSessionState::TakenOver);
    assert!(
        world.takeover.verify(&token, ts(5_000)).await.is_ok(),
        "the operator credential survives the foreign attempt"
    );
}

/// Helpers on [`World`] that keep the tests readable.
impl World {
    async fn state_of(&self, session: &AuthSessionId) -> AuthSessionState {
        self.coordinator
            .get(&self.tenant, session)
            .await
            .expect("session get")
            .state
    }
}
