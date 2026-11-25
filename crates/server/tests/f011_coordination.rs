//! F011 acceptance tests: the session coordinator's cross-session rules
//! (mutual exclusion, credential pins, manual takeover) against a real
//! database. Skipped when `FISHPOND_TEST_DATABASE_URL` is unset.

mod common;

use std::time::Duration;

use common::{account, gated, tenant_record, ts, unique_entity, unique_tenant};
use fishpond_domain::{AccountId, AuthSessionId, AuthSessionState, TenantId, UnixMillis};
use fishpond_runtime::{ActorBehavior, ActorError, MailboxError, MailboxSender};
use fishpond_server::{SessionCoordinator, Sessions};
use fishpond_storage::{CredentialCipher, MasterKey, NewCredential, SecretBox, Store};

const SECRET_V1: &str = "f011-secret-generation-one";
const SECRET_V2: &str = "f011-secret-generation-two";

/// A do-nothing automation behavior — the takeover test only cares about
/// the actor's lifecycle, not its work.
#[derive(Default)]
struct Echo;

impl ActorBehavior for Echo {
    type Message = u32;

    fn reset(&mut self) {}

    async fn handle(&mut self, _message: u32) -> Result<(), ActorError> {
        Ok(())
    }
}

/// Everything one test needs: a tenant, an account, a v1 credential and
/// the services on top.
struct World {
    tenant: TenantId,
    account: AccountId,
    credential: String,
    store: Store,
    sessions: Sessions,
    coordinator: SessionCoordinator,
}

impl World {
    /// Builds a fresh tenant/account/credential world; skipped when CI
    /// has no database.
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
        Some(World {
            tenant,
            account: account_id,
            credential,
            store,
            sessions: sessions.clone(),
            coordinator: SessionCoordinator::new(sessions),
        })
    }

    /// Starts a fresh `PENDING` session for the world's account.
    async fn start_session(&self, tag: &str) -> AuthSessionId {
        let id = AuthSessionId::parse(unique_entity("sess", tag)).expect("session id");
        self.sessions
            .start(
                &self.tenant,
                id.clone(),
                self.account.clone(),
                ts(1_000),
                None,
            )
            .await
            .expect("session start");
        id
    }

    /// Authorizes `id` against the world's credential, pinning the
    /// credential's *current* generation (as the real login path does).
    async fn authorize_current(&self, id: &AuthSessionId, expires_at: Option<UnixMillis>) {
        let version = self
            .sessions
            .credential_version(&self.tenant, &self.credential)
            .await
            .expect("credential version");
        self.coordinator
            .authorize_exclusive(
                &self.tenant,
                id,
                &self.credential,
                version,
                expires_at,
                ts(2_000),
            )
            .await
            .expect("exclusive authorize");
    }
}

/// Polls until the sender's mailbox is closed (the actor task has ended)
/// or the deadline passes, returning the terminal error.
async fn wait_closed(tx: &MailboxSender<u32>) -> MailboxError {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    loop {
        match tx.try_send(1) {
            Err(error) => return error,
            Ok(()) if tokio::time::Instant::now() >= deadline => {
                panic!("mailbox never closed within the deadline")
            }
            Ok(()) => tokio::time::sleep(Duration::from_millis(5)).await,
        }
    }
}

#[tokio::test]
async fn only_one_live_session_per_account_is_allowed() {
    let Some(world) = World::new("f011-exclusive").await else {
        return;
    };
    let first = world.start_session("f011-exclusive-a").await;
    let second = world.start_session("f011-exclusive-b").await;
    world.authorize_current(&first, None).await;
    assert_eq!(
        world
            .coordinator
            .get(&world.tenant, &first)
            .await
            .unwrap()
            .state,
        AuthSessionState::Authorized
    );

    // A second exclusive authorize must lose.
    let error = world
        .coordinator
        .authorize_exclusive(
            &world.tenant,
            &second,
            &world.credential,
            1,
            None,
            ts(3_000),
        )
        .await
        .expect_err("second live session must conflict");
    assert!(error.to_string().contains("already has a live session"));

    // A pending sibling does not block either: the stale-login rule.
    let third = world.start_session("f011-exclusive-c").await;
    world
        .sessions
        .revoke(&world.tenant, &first, Some("rotate".into()), ts(3_100))
        .await
        .unwrap();
    world.authorize_current(&third, None).await;
    assert_eq!(
        world
            .coordinator
            .get(&world.tenant, &third)
            .await
            .unwrap()
            .state,
        AuthSessionState::Authorized
    );
    // The loser never moved.
    assert_eq!(
        world
            .coordinator
            .get(&world.tenant, &second)
            .await
            .unwrap()
            .state,
        AuthSessionState::Pending
    );
}

#[tokio::test]
async fn concurrent_authorizes_serialize_through_the_account_gate() {
    let Some(world) = World::new("f011-race").await else {
        return;
    };
    let first = world.start_session("f011-race-a").await;
    let second = world.start_session("f011-race-b").await;
    let credential = world.credential.clone();
    let tenant = world.tenant.clone();

    let (left, right) = tokio::join!(
        world
            .coordinator
            .authorize_exclusive(&tenant, &first, &credential, 1, None, ts(2_000)),
        world
            .coordinator
            .authorize_exclusive(&tenant, &second, &credential, 1, None, ts(2_000)),
    );
    // The gate makes the check-then-act window atomic: exactly one
    // winner regardless of poll order.
    let (winner, loser) = match (left, right) {
        (Ok(_), Err(_)) => (&first, &second),
        (Err(_), Ok(_)) => (&second, &first),
        (Ok(_), Ok(_)) => panic!("both concurrent authorizes won"),
        (Err(_), Err(_)) => panic!("both concurrent authorizes lost"),
    };
    assert_eq!(
        world.coordinator.get(&tenant, winner).await.unwrap().state,
        AuthSessionState::Authorized
    );
    assert_eq!(
        world.coordinator.get(&tenant, loser).await.unwrap().state,
        AuthSessionState::Pending
    );
}

#[tokio::test]
async fn credential_rotation_expires_the_session_and_demands_reauth() {
    let Some(world) = World::new("f011-rotation").await else {
        return;
    };
    let session = world.start_session("f011-rotation-a").await;
    world.authorize_current(&session, None).await;
    let pinned = world
        .sessions
        .credential_version(&world.tenant, &world.credential)
        .await
        .unwrap();
    world
        .coordinator
        .ensure_automatable(&world.tenant, &world.account)
        .await
        .expect("fresh pin is current");

    // Rotate the envelope to generation `pinned + 1` underneath the
    // session (the kek-rotation path from F008).
    let rotated = CredentialCipher::new(MasterKey::generate(), pinned + 1);
    world
        .store
        .replace_credential_envelope(
            &world.tenant,
            &world.credential,
            &rotated.seal(SECRET_V2).expect("seal v2"),
        )
        .await
        .expect("envelope replace");

    let error = world
        .coordinator
        .ensure_automatable(&world.tenant, &world.account)
        .await
        .expect_err("rotated pin must fail");
    let fishpond_server::ServiceError::ReauthRequired {
        session: expired,
        pinned: reported_pinned,
        current: reported_current,
    } = &error
    else {
        panic!("expected ReauthRequired, got {error}");
    };
    assert_eq!(expired, session.as_str());
    assert_eq!(*reported_pinned, pinned);
    assert_eq!(*reported_current, pinned + 1);
    assert!(error.to_string().contains("re-authentication required"));

    // The coordinator expired the session as part of the rejection.
    assert_eq!(
        world
            .coordinator
            .get(&world.tenant, &session)
            .await
            .unwrap()
            .state,
        AuthSessionState::Expired
    );

    // EXPIRED is terminal: a fresh login is the only way back. A new
    // session authorized after rotation pins the *new* generation and
    // automation is allowed again.
    let fresh = world.start_session("f011-rotation-b").await;
    world.authorize_current(&fresh, None).await;
    assert_eq!(
        world
            .coordinator
            .get(&world.tenant, &fresh)
            .await
            .unwrap()
            .state,
        AuthSessionState::Authorized
    );
    assert!(
        world
            .coordinator
            .ensure_automatable(&world.tenant, &world.account)
            .await
            .is_ok()
    );
}

#[tokio::test]
async fn takeover_stops_the_actor_and_blocks_automation_until_resume() {
    let Some(world) = World::new("f011-takeover").await else {
        return;
    };
    let session = world.start_session("f011-takeover-a").await;
    world
        .sessions
        .mark_confirmation(&world.tenant, &session, None, ts(1_500))
        .await
        .expect("confirmation step");
    world.authorize_current(&session, None).await;

    let mailbox = world
        .coordinator
        .attach::<Echo>(&world.tenant, world.account.clone(), Echo, 4)
        .await
        .expect("attach with live authorized session");
    mailbox.try_send(1).expect("mailbox open after attach");
    assert!(world.coordinator.is_attached(&world.account).await);

    // Takeover flips the state and stops the actor.
    let taken = world
        .coordinator
        .take_over(
            &world.tenant,
            &session,
            Some("operator pause".into()),
            ts(3_000),
        )
        .await
        .expect("takeover");
    assert_eq!(taken.state, AuthSessionState::TakenOver);
    assert!(!world.coordinator.is_attached(&world.account).await);
    assert!(matches!(wait_closed(&mailbox).await, MailboxError::Closed));

    // Automation is refused while taken over.
    let error = world
        .coordinator
        .ensure_automatable(&world.tenant, &world.account)
        .await
        .expect_err("taken-over session must not automate");
    assert!(error.to_string().contains("manual takeover"));
    assert!(
        world
            .coordinator
            .attach::<Echo>(&world.tenant, world.account.clone(), Echo, 4)
            .await
            .is_err()
    );

    // A second takeover is an illegal transition (machine decides).
    assert!(matches!(
        world
            .coordinator
            .take_over(&world.tenant, &session, None, ts(3_100))
            .await,
        Err(fishpond_server::ServiceError::Domain(_))
    ));

    // Resume re-grants permission; the actor is re-attached fresh.
    let resumed = world
        .coordinator
        .resume(&world.tenant, &session, ts(3_200))
        .await
        .expect("resume");
    assert_eq!(resumed.state, AuthSessionState::Authorized);
    let second_mailbox = world
        .coordinator
        .attach::<Echo>(&world.tenant, world.account.clone(), Echo, 4)
        .await
        .expect("re-attach after resume");
    second_mailbox.try_send(2).expect("new mailbox open");

    // Takeover from a PENDING session is an illegal transition.
    let pending = world.start_session("f011-takeover-b").await;
    assert!(matches!(
        world
            .coordinator
            .take_over(&world.tenant, &pending, None, ts(3_300))
            .await,
        Err(fishpond_server::ServiceError::Domain(_))
    ));
}

#[tokio::test]
async fn expire_stale_sweeps_only_overdue_sessions() {
    let Some(world) = World::new("f011-stale").await else {
        return;
    };
    let now = ts(10_000);
    // One already past its TTL, one with headroom.
    let overdue = world.start_session("f011-stale-a").await;
    world.authorize_current(&overdue, Some(ts(9_900))).await;
    let fresh = world.start_session("f011-stale-b").await;

    // The stale-but-authorized session still holds the exclusion until
    // the sweep runs — that is what the sweep is for.
    let error = world
        .coordinator
        .authorize_exclusive(
            &world.tenant,
            &fresh,
            &world.credential,
            1,
            Some(ts(20_000)),
            now,
        )
        .await
        .expect_err("stale holder still excludes");
    assert!(error.to_string().contains("already has a live session"));

    let swept = world
        .coordinator
        .expire_stale(&world.tenant, now)
        .await
        .expect("sweep");
    assert_eq!(swept, 1);
    assert_eq!(
        world
            .coordinator
            .get(&world.tenant, &overdue)
            .await
            .unwrap()
            .state,
        AuthSessionState::Expired
    );

    // After the sweep the slot is free.
    world
        .coordinator
        .authorize_exclusive(
            &world.tenant,
            &fresh,
            &world.credential,
            1,
            Some(ts(20_000)),
            now,
        )
        .await
        .expect("fresh session authorizes after sweep");

    // Idempotent: the expired session is not swept again; the second
    // session still has TTL headroom at `now` and is left alone.
    assert_eq!(
        world
            .coordinator
            .expire_stale(&world.tenant, now)
            .await
            .expect("second sweep"),
        0
    );
    assert_eq!(
        world
            .coordinator
            .get(&world.tenant, &fresh)
            .await
            .unwrap()
            .state,
        AuthSessionState::Authorized
    );
}
