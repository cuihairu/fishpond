//! F009 acceptance: authorization-session persistence — lifecycle rows,
//! credential binding, and tenant scoping under row-level security.

mod common;

use common::*;

use fishpond_domain::{AccountId, AuthSessionId, AuthSessionState, TenantId, UnixMillis};
use fishpond_storage::{
    AuthSessionUpdate, CredentialCipher, MasterKey, NewAuthSession, NewCredential, SecretBox,
};

fn new_session(id: AuthSessionId, tenant: &TenantId, account: &AccountId) -> NewAuthSession {
    NewAuthSession {
        id,
        tenant: tenant.clone(),
        account_id: account.clone(),
        platform: "mock".to_owned(),
        state: AuthSessionState::Pending,
        created_at: ts(1_000),
        expires_at: Some(ts(9_000)),
    }
}

#[tokio::test]
async fn session_lifecycle_persists_state_and_bindings() {
    let Some(store) = gated().await else {
        eprintln!("skip: {ENV} is not set (CI has no database)");
        return;
    };
    let tenant = unique_tenant("sess");
    store
        .upsert_tenant(&tenant_record(&tenant, "Sessions"))
        .await
        .unwrap();
    let account_id = AccountId::parse(unique_entity("acc", "sess")).unwrap();
    store
        .upsert_account(&account(
            account_id.as_str(),
            &tenant,
            state_offline(),
            None,
        ))
        .await
        .unwrap();

    // The session binds a *real* credential, exercising the F008 seam.
    let cipher = CredentialCipher::new(MasterKey::generate(), 1);
    let credential_id = unique_entity("cred", "sess");
    store
        .store_credential(NewCredential {
            id: &credential_id,
            tenant: &tenant,
            platform: "mock",
            kind: "api_token",
            cipher: &cipher,
            secret: SecretBox::new("sess-test-secret"),
            created_at: UnixMillis::from_millis(900).unwrap(),
        })
        .await
        .unwrap();

    let id = AuthSessionId::parse(unique_entity("sess", "life")).unwrap();
    store
        .insert_auth_session(&new_session(id.clone(), &tenant, &account_id))
        .await
        .unwrap();

    let started = store.get_auth_session(&tenant, &id).await.unwrap();
    assert_eq!(started.state, AuthSessionState::Pending);
    assert_eq!(started.account_id, account_id);
    assert_eq!(started.expires_at, Some(ts(9_000)));
    assert!(started.credential_id.is_none());

    // confirm → authorize, binding the credential and its envelope version.
    let note = "user scanned the code".to_owned();
    assert!(
        store
            .update_auth_session(
                &tenant,
                &id,
                &AuthSessionUpdate {
                    state: Some(AuthSessionState::AwaitingConfirmation),
                    state_note: Some(note.clone()),
                    ..AuthSessionUpdate::default()
                },
                ts(2_000),
            )
            .await
            .unwrap()
    );
    let confirming = store.get_auth_session(&tenant, &id).await.unwrap();
    assert_eq!(confirming.state, AuthSessionState::AwaitingConfirmation);
    assert_eq!(confirming.state_note.as_deref(), Some(note.as_str()));
    assert_eq!(confirming.updated_at, ts(2_000));

    assert!(
        store
            .update_auth_session(
                &tenant,
                &id,
                &AuthSessionUpdate {
                    state: Some(AuthSessionState::Authorized),
                    state_note: Some(String::new()),
                    clear_note: true,
                    credential_id: Some(credential_id.clone()),
                    credential_version: Some(1),
                    ..AuthSessionUpdate::default()
                },
                ts(3_000),
            )
            .await
            .unwrap()
    );
    let authorized = store.get_auth_session(&tenant, &id).await.unwrap();
    assert_eq!(authorized.state, AuthSessionState::Authorized);
    assert_eq!(authorized.state_note, None, "clear_note must wipe the note");
    assert_eq!(
        authorized.credential_id.as_deref(),
        Some(credential_id.as_str())
    );
    assert_eq!(authorized.credential_version, Some(1));

    // Only the bound columns move on a partial update.
    assert!(
        store
            .update_auth_session(
                &tenant,
                &id,
                &AuthSessionUpdate::state(AuthSessionState::Revoked),
                ts(4_000),
            )
            .await
            .unwrap()
    );
    let revoked = store.get_auth_session(&tenant, &id).await.unwrap();
    assert_eq!(revoked.state, AuthSessionState::Revoked);
    assert_eq!(
        revoked.credential_id.as_deref(),
        Some(credential_id.as_str()),
        "binding survives"
    );
    assert_eq!(revoked.credential_version, Some(1));
    assert_eq!(revoked.expires_at, Some(ts(9_000)), "expiry survives");
    assert_eq!(revoked.updated_at, ts(4_000));

    // Listing filters by account and orders oldest first.
    let second = AuthSessionId::parse(unique_entity("sess", "life2")).unwrap();
    store
        .insert_auth_session(&new_session(second.clone(), &tenant, &account_id))
        .await
        .unwrap();
    assert_eq!(
        store
            .list_auth_sessions(&tenant, Some(&account_id))
            .await
            .unwrap()
            .len(),
        2
    );

    // Delete removes exactly the targeted row.
    assert!(store.delete_auth_session(&tenant, &id).await.unwrap());
    assert!(!store.delete_auth_session(&tenant, &id).await.unwrap());
    assert_eq!(
        store
            .list_auth_sessions(&tenant, Some(&account_id))
            .await
            .unwrap()
            .len(),
        1
    );
}

#[tokio::test]
async fn sessions_are_tenant_scoped_under_rls() {
    let Some(store) = gated().await else {
        eprintln!("skip: {ENV} is not set (CI has no database)");
        return;
    };
    let tenant_a = unique_tenant("sess-a");
    let tenant_b = unique_tenant("sess-b");
    store
        .upsert_tenant(&tenant_record(&tenant_a, "A"))
        .await
        .unwrap();
    store
        .upsert_tenant(&tenant_record(&tenant_b, "B"))
        .await
        .unwrap();
    let account_a = AccountId::parse(unique_entity("acc", "sess-a")).unwrap();
    store
        .upsert_account(&account(
            account_a.as_str(),
            &tenant_a,
            state_offline(),
            None,
        ))
        .await
        .unwrap();

    let id = AuthSessionId::parse(unique_entity("sess", "scope")).unwrap();
    store
        .insert_auth_session(&new_session(id.clone(), &tenant_a, &account_a))
        .await
        .unwrap();

    // B cannot read, update or delete A's session (RLS hides the row).
    match store.get_auth_session(&tenant_b, &id).await {
        Err(fishpond_storage::StorageError::NotFound { what, .. }) => {
            assert_eq!(what, "auth session");
        }
        other => panic!("tenant B must not see A's session, got {other:?}"),
    }
    assert!(
        store
            .list_auth_sessions(&tenant_b, None)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        !store
            .update_auth_session(
                &tenant_b,
                &id,
                &AuthSessionUpdate::state(AuthSessionState::Revoked),
                ts(5_000),
            )
            .await
            .unwrap()
    );
    assert!(!store.delete_auth_session(&tenant_b, &id).await.unwrap());

    // A is untouched by all of B's attempts.
    assert_eq!(
        store.get_auth_session(&tenant_a, &id).await.unwrap().state,
        AuthSessionState::Pending
    );

    // Unbound (raw pool) reads fail closed.
    let rows = sqlx::query_scalar::<_, String>("SELECT id FROM auth_sessions")
        .fetch_all(store.pool())
        .await
        .unwrap();
    assert!(rows.is_empty(), "unbound pool access must see no sessions");
}

/// `AccountState::Offline`, spelled out to keep the import list short.
fn state_offline() -> fishpond_domain::AccountState {
    fishpond_domain::AccountState::Offline
}
