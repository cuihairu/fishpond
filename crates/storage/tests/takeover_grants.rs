//! F034 storage face: short-lived takeover grants. A grant is the
//! operator's credential for driving a taken-over session remotely —
//! issued once, verified by presenting the raw token, revoked on
//! release. The database holds only the SHA-256 hash and a display
//! tail; the raw token exists solely in the issue response. Skipped
//! when `FISHPOND_TEST_DATABASE_URL` is unset.

mod common;

use common::{ENV, account, db_url, gated, tenant_record, ts, unique_entity, unique_tenant};
use fishpond_domain::{AccountId, AuthSessionId, AuthSessionState, TenantId};
use fishpond_storage::{
    NewAuthSession, NewTakeoverGrant, StorageError, Store, generate_takeover_token,
    hash_takeover_token,
};

/// A tenant with one account and one authorization session, ready to
/// hold grants. Grants reference real sessions (FK), so the fixture
/// mints one.
struct World {
    store: Store,
    tenant: TenantId,
    account_id: String,
    session: String,
}

async fn world(tag: &str) -> Option<World> {
    let store = gated().await?;
    let tenant = unique_tenant(tag);
    store
        .upsert_tenant(&tenant_record(&tenant, tag))
        .await
        .expect("tenant upsert");
    let account_id = unique_entity("acc", tag);
    store
        .upsert_account(&account(
            &account_id,
            &tenant,
            fishpond_domain::AccountState::Online,
            None,
        ))
        .await
        .expect("account upsert");
    let session = unique_entity("sess", tag);
    store
        .insert_auth_session(&NewAuthSession {
            id: AuthSessionId::parse(&session).expect("session id is valid"),
            tenant: tenant.clone(),
            account_id: AccountId::parse(&account_id).expect("account id is valid"),
            platform: "mock".to_owned(),
            state: AuthSessionState::Authorized,
            created_at: ts(1_000),
            expires_at: None,
        })
        .await
        .expect("session insert");
    Some(World {
        store,
        tenant,
        account_id,
        session,
    })
}

impl World {
    /// Mints one more authorization session for the same account.
    async fn extra_session(&self, tag: &str) -> String {
        let session = unique_entity("sess", tag);
        self.store
            .insert_auth_session(&NewAuthSession {
                id: AuthSessionId::parse(&session).expect("session id is valid"),
                tenant: self.tenant.clone(),
                account_id: AccountId::parse(&self.account_id).expect("account id is valid"),
                platform: "mock".to_owned(),
                state: AuthSessionState::Authorized,
                created_at: ts(1_000),
                expires_at: None,
            })
            .await
            .expect("session insert");
        session
    }

    /// Issues one 60s grant against the world's session; returns the
    /// raw token and the grant id.
    async fn issue(&self, tag: &str) -> (String, String) {
        self.issue_on(&self.session, tag).await
    }

    /// Issues one 60s grant against an arbitrary session.
    async fn issue_on(&self, session: &str, tag: &str) -> (String, String) {
        let token = generate_takeover_token();
        let grant = unique_entity("grant", tag);
        self.store
            .issue_takeover_grant(&NewTakeoverGrant {
                id: &grant,
                tenant: &self.tenant,
                account: &self.account_id,
                session,
                operator: &format!("op-{tag}"),
                token: &token,
                created_at: ts(1_000),
                expires_at: ts(61_000),
            })
            .await
            .expect("grant issue");
        (token, grant)
    }
}

#[tokio::test]
async fn grant_lifecycle_issue_verify_revoke() {
    let Some(world) = world("f034-life").await else {
        eprintln!("skip: {ENV} is not set (CI has no database)");
        return;
    };
    let (token, grant) = world.issue("life").await;

    // Verify resolves the grant by raw token possession alone.
    let seen = world
        .store
        .verify_takeover_token(&token, ts(2_000))
        .await
        .expect("verify")
        .expect("live grant verifies");
    assert_eq!(seen.id, grant);
    assert_eq!(seen.tenant_id, world.tenant.as_str());
    assert_eq!(seen.account_id, world.account_id);
    assert_eq!(seen.session_id, world.session);
    assert_eq!(seen.operator, "op-life");
    assert_eq!(seen.expires_at, ts(61_000));
    assert_eq!(seen.revoked_at, None);

    // Revocation is tenant-scoped and then the token stops working.
    assert!(
        world
            .store
            .revoke_takeover_grant(&world.tenant, &grant, ts(3_000))
            .await
            .expect("revoke"),
        "the live grant must revoke"
    );
    assert!(
        world
            .store
            .verify_takeover_token(&token, ts(4_000))
            .await
            .expect("verify after revoke")
            .is_none(),
        "a revoked grant must not verify"
    );
    // A second revoke is a no-op, not an error.
    assert!(
        !world
            .store
            .revoke_takeover_grant(&world.tenant, &grant, ts(5_000))
            .await
            .expect("second revoke"),
        "revoking twice must report false"
    );
}

#[tokio::test]
async fn live_grants_are_unique_per_session() {
    let Some(world) = world("f034-unique").await else {
        eprintln!("skip: {ENV} is not set (CI has no database)");
        return;
    };
    let session = world.extra_session("unique-sess").await;
    let (_token, grant) = world.issue_on(&session, "unique").await;

    // A second live grant for the same session is a named conflict.
    let second = world
        .store
        .issue_takeover_grant(&NewTakeoverGrant {
            id: &unique_entity("grant", "unique-2"),
            tenant: &world.tenant,
            account: &world.account_id,
            session: &session,
            operator: "op-unique-2",
            token: &generate_takeover_token(),
            created_at: ts(2_000),
            expires_at: ts(62_000),
        })
        .await;
    assert!(
        matches!(second, Err(StorageError::Conflict { .. })),
        "second live grant must conflict, got {second:?}"
    );

    // After revocation the session can take a fresh grant — the
    // uniqueness binds live rows only.
    world
        .store
        .revoke_takeover_grant(&world.tenant, &grant, ts(3_000))
        .await
        .expect("revoke first");
    world
        .store
        .issue_takeover_grant(&NewTakeoverGrant {
            id: &unique_entity("grant", "unique-3"),
            tenant: &world.tenant,
            account: &world.account_id,
            session: &session,
            operator: "op-unique-3",
            token: &generate_takeover_token(),
            created_at: ts(4_000),
            expires_at: ts(64_000),
        })
        .await
        .expect("re-issue after revoke");
}

#[tokio::test]
async fn expired_grants_stop_verifying() {
    let Some(world) = world("f034-expiry").await else {
        eprintln!("skip: {ENV} is not set (CI has no database)");
        return;
    };
    let token = generate_takeover_token();
    world
        .store
        .issue_takeover_grant(&NewTakeoverGrant {
            id: &unique_entity("grant", "expiry"),
            tenant: &world.tenant,
            account: &world.account_id,
            session: &world.session,
            operator: "op-expiry",
            token: &token,
            created_at: ts(1_000),
            expires_at: ts(2_000),
        })
        .await
        .expect("grant issue");

    let live = world
        .store
        .verify_takeover_token(&token, ts(1_999))
        .await
        .expect("verify before expiry");
    assert!(live.is_some(), "one millisecond before expiry still works");
    let dead = world
        .store
        .verify_takeover_token(&token, ts(2_000))
        .await
        .expect("verify at expiry");
    assert!(dead.is_none(), "the expiry instant itself is past");
}

#[tokio::test]
async fn grant_administration_is_tenant_scoped() {
    let Some(world) = world("f034-scope").await else {
        eprintln!("skip: {ENV} is not set (CI has no database)");
        return;
    };
    let other = unique_tenant("f034-scope-other");
    world
        .store
        .upsert_tenant(&tenant_record(&other, "f034-scope-other"))
        .await
        .expect("other tenant upsert");
    let (token, grant) = world.issue("scope").await;

    // Another tenant sees nothing through the administration surface.
    let foreign_list = world
        .store
        .list_takeover_grants(&other, &world.session)
        .await
        .expect("foreign list");
    assert!(foreign_list.is_empty());
    let foreign_revoke = world
        .store
        .revoke_takeover_grant(&other, &grant, ts(3_000))
        .await
        .expect("foreign revoke");
    assert!(!foreign_revoke, "a foreign tenant must not revoke");
    // The grant still verifies — the owner never touched it.
    assert!(
        world
            .store
            .verify_takeover_token(&token, ts(4_000))
            .await
            .expect("verify")
            .is_some(),
        "foreign revoke must not have touched the grant"
    );

    // The owner's own administration surface sees its grant.
    let mine = world
        .store
        .list_takeover_grants(&world.tenant, &world.session)
        .await
        .expect("owner list");
    assert_eq!(mine.len(), 1);
    assert_eq!(mine[0].id, grant);
}

#[tokio::test]
async fn stored_rows_hold_hashes_not_tokens() {
    let Some(world) = world("f034-hash").await else {
        eprintln!("skip: {ENV} is not set (CI has no database)");
        return;
    };
    let (token, grant) = world.issue("hash").await;

    // The hash surface is derivable and the raw token is nowhere in it.
    assert_eq!(hash_takeover_token(&token).len(), 64);
    assert_ne!(hash_takeover_token(&token), token);
    let row = world
        .store
        .list_takeover_grants(&world.tenant, &world.session)
        .await
        .expect("list")
        .pop()
        .expect("the grant row");
    assert_eq!(row.id, grant);
    assert_ne!(row.token_hash, token, "the raw token must not be the hash");
    assert!(
        !row.token_hint.contains(&token),
        "hint must not carry raw material"
    );
    assert!(
        token.ends_with(&row.token_hint),
        "hint is the display tail of the raw token"
    );

    // Raw database reads (no tenant bound) still expose only hashes —
    // the auth-time surface holds no secret material.
    let raw = Store::connect_unmigrated(&db_url().expect("db url"))
        .await
        .expect("raw pool");
    let hashes: Vec<String> = sqlx::query_scalar("SELECT token_hash FROM takeover_grants")
        .fetch_all(raw.pool())
        .await
        .expect("raw scan");
    assert!(hashes.iter().all(|h| !h.contains(&token)));
}

#[tokio::test]
async fn unknown_tokens_verify_to_nothing() {
    let Some(world) = world("f034-unknown").await else {
        eprintln!("skip: {ENV} is not set (CI has no database)");
        return;
    };
    let stranger = generate_takeover_token();
    let seen = world
        .store
        .verify_takeover_token(&stranger, ts(1_000))
        .await
        .expect("verify unknown");
    assert!(seen.is_none(), "an unissued token verifies to nothing");
}
