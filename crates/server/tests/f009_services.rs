//! F009 acceptance: account management, authorization sessions and the
//! status API — with illegal-transition, tenant-isolation and
//! secret-safety assertions. Runs against the env-gated test database,
//! skipping when CI has none.

mod common;

use common::*;

use fishpond_domain::{AccountId, AccountState, AuthSessionId, AuthSessionState};
use fishpond_server::{Accounts, ServiceError, Sessions};
use fishpond_storage::{CredentialCipher, MasterKey, NewCredential, SecretBox};

#[tokio::test]
async fn account_registration_status_and_state_machine() {
    let Some(store) = gated().await else {
        eprintln!("skip: {ENV} is not set (CI has no database)");
        return;
    };
    let tenant = unique_tenant("svc-acc");
    store
        .upsert_tenant(&tenant_record(&tenant, "Accounts"))
        .await
        .unwrap();
    let accounts = Accounts::new(store);

    let id = AccountId::parse(unique_entity("acc", "svc")).unwrap();
    let registered = accounts
        .register(&tenant, id.clone(), "mock", ts(1_000))
        .await
        .unwrap();
    assert_eq!(registered.state, AccountState::Offline);
    assert!(accounts.exists(&tenant, &id).await.unwrap());

    // Registering twice is a conflict, not a silent overwrite.
    let dup = accounts
        .register(&tenant, id.clone(), "mock", ts(1_100))
        .await
        .unwrap_err();
    assert!(matches!(dup, ServiceError::Conflict { .. }), "got {dup:?}");

    // OFFLINE → ONLINE → UNKNOWN (observable but not verified).
    accounts
        .set_state(&tenant, &id, AccountState::Online, None, ts(2_000))
        .await
        .unwrap();
    let observed = accounts
        .set_state(
            &tenant,
            &id,
            AccountState::Unknown,
            Some("probe timed out".to_owned()),
            ts(3_000),
        )
        .await
        .unwrap();
    assert_eq!(observed.state, AccountState::Unknown);
    assert_eq!(observed.state_note.as_deref(), Some("probe timed out"));

    let summary = accounts.status(&tenant).await.unwrap();
    assert_eq!(summary.total, 1);
    assert_eq!(summary.unknown, 1);
    assert_eq!(summary.online, 0);
    // UNKNOWN is surfaced as-is — never silently mapped to offline.
    assert_eq!(
        accounts.get(&tenant, &id).await.unwrap().state,
        AccountState::Unknown
    );

    // Same-state "change" is an illegal no-op (the machine rejects it).
    let noop = accounts
        .set_state(&tenant, &id, AccountState::Unknown, None, ts(4_000))
        .await
        .unwrap_err();
    assert!(matches!(noop, ServiceError::Domain(_)), "got {noop:?}");
    assert!(noop.to_string().contains("UNKNOWN -> UNKNOWN"));

    // DISABLED is terminal: no exit even for a reconcile.
    accounts
        .set_state(&tenant, &id, AccountState::Disabled, None, ts(5_000))
        .await
        .unwrap();
    let illegal = accounts
        .set_state(&tenant, &id, AccountState::Online, None, ts(6_000))
        .await
        .unwrap_err();
    assert!(
        matches!(illegal, ServiceError::Domain(_)),
        "got {illegal:?}"
    );
    assert_eq!(accounts.status(&tenant).await.unwrap().disabled, 1);
}

#[tokio::test]
async fn sessions_follow_the_machine_and_bind_a_credential() {
    let Some(store) = gated().await else {
        eprintln!("skip: {ENV} is not set (CI has no database)");
        return;
    };
    let tenant = unique_tenant("svc-sess");
    store
        .upsert_tenant(&tenant_record(&tenant, "Sessions"))
        .await
        .unwrap();
    let account_id = AccountId::parse(unique_entity("acc", "svc-sess")).unwrap();
    store
        .upsert_account(&account(
            account_id.as_str(),
            &tenant,
            AccountState::Offline,
        ))
        .await
        .unwrap();

    // A live credential is required before a session can be authorized.
    let cipher = CredentialCipher::new(MasterKey::generate(), 7);
    let credential_id = unique_entity("cred", "svc-sess");
    store
        .store_credential(NewCredential {
            id: &credential_id,
            tenant: &tenant,
            platform: "mock",
            kind: "api_token",
            cipher: &cipher,
            secret: SecretBox::new("svc-session-secret-do-not-log"),
            created_at: ts(900),
        })
        .await
        .unwrap();

    let sessions = Sessions::new(store.clone());
    let id = AuthSessionId::parse(unique_entity("sess", "svc")).unwrap();
    let started = sessions
        .start(
            &tenant,
            id.clone(),
            account_id.clone(),
            ts(1_000),
            Some(ts(9_000)),
        )
        .await
        .unwrap();
    assert_eq!(started.state, AuthSessionState::Pending);
    assert_eq!(started.platform, "mock");

    // PENDING → AWAITING_CONFIRMATION → AUTHORIZED (with version pin).
    sessions
        .mark_confirmation(&tenant, &id, Some("scanned".to_owned()), ts(2_000))
        .await
        .unwrap();
    let authorized = sessions
        .authorize(&tenant, &id, &credential_id, 7, Some(ts(9_500)), ts(3_000))
        .await
        .unwrap();
    assert_eq!(authorized.state, AuthSessionState::Authorized);
    assert_eq!(
        authorized.credential_id.as_deref(),
        Some(credential_id.as_str())
    );
    assert_eq!(authorized.credential_version, Some(7));
    assert_eq!(authorized.expires_at, Some(ts(9_500)));
    assert_eq!(
        sessions
            .live_for_account(&tenant, &account_id, ts(4_000))
            .await
            .unwrap()
            .len(),
        1
    );

    // Illegal transitions are refused by the domain machine (no revive
    // after EXPIRED).
    let second = AuthSessionId::parse(unique_entity("sess", "svc2")).unwrap();
    sessions
        .start(&tenant, second.clone(), account_id.clone(), ts(1_000), None)
        .await
        .unwrap();
    sessions
        .authorize(&tenant, &second, &credential_id, 7, None, ts(1_100))
        .await
        .unwrap();
    assert_eq!(
        sessions
            .expire(&tenant, &second, ts(1_200))
            .await
            .unwrap()
            .state,
        AuthSessionState::Expired
    );
    let revive = sessions
        .expire(&tenant, &second, ts(1_300))
        .await
        .unwrap_err();
    assert!(matches!(revive, ServiceError::Domain(_)), "got {revive:?}");
    assert!(
        sessions
            .revoke(&tenant, &second, None, ts(1_400))
            .await
            .is_err(),
        "terminal states have no exits"
    );
    // Only the still-live first session counts for automation.
    assert_eq!(
        sessions
            .live_for_account(&tenant, &account_id, ts(9_000))
            .await
            .unwrap()
            .len(),
        1
    );
    assert!(
        sessions
            .live_for_account(&tenant, &account_id, ts(9_600))
            .await
            .unwrap()
            .is_empty(),
        "an expired session must not be live"
    );

    // A revoked credential cannot be bound to a new session.
    store
        .revoke_credential(&tenant, &credential_id, ts(9_000))
        .await
        .unwrap();
    let fresh = AuthSessionId::parse(unique_entity("sess", "svc3")).unwrap();
    sessions
        .start(&tenant, fresh.clone(), account_id.clone(), ts(9_100), None)
        .await
        .unwrap();
    let err = sessions
        .authorize(&tenant, &fresh, &credential_id, 7, None, ts(9_200))
        .await
        .unwrap_err();
    assert!(matches!(err, ServiceError::NotFound { what, .. } if what == "credential"));
}

#[tokio::test]
async fn services_are_tenant_scoped_and_never_echo_secrets() {
    let Some(store) = gated().await else {
        eprintln!("skip: {ENV} is not set (CI has no database)");
        return;
    };
    let tenant_a = unique_tenant("svc-a");
    let tenant_b = unique_tenant("svc-b");
    store
        .upsert_tenant(&tenant_record(&tenant_a, "A"))
        .await
        .unwrap();
    store
        .upsert_tenant(&tenant_record(&tenant_b, "B"))
        .await
        .unwrap();
    let account_a = AccountId::parse(unique_entity("acc", "svc-a")).unwrap();
    Accounts::new(store.clone())
        .register(&tenant_a, account_a.clone(), "mock", ts(1_000))
        .await
        .unwrap();

    let accounts_a = Accounts::new(store.clone());
    let accounts_b = Accounts::new(store.clone());
    assert_eq!(accounts_a.list(&tenant_b).await.unwrap().len(), 0);
    let not_found = accounts_b.get(&tenant_b, &account_a).await.unwrap_err();
    assert!(matches!(not_found, ServiceError::NotFound { .. }));
    assert!(!accounts_b.exists(&tenant_b, &account_a).await.unwrap());

    let sessions_a = Sessions::new(store.clone());
    let sessions_b = Sessions::new(store);
    let id = AuthSessionId::parse(unique_entity("sess", "svc-iso")).unwrap();
    sessions_a
        .start(&tenant_a, id.clone(), account_a.clone(), ts(2_000), None)
        .await
        .unwrap();
    let not_found = sessions_b.get(&tenant_b, &id).await.unwrap_err();
    assert!(matches!(not_found, ServiceError::NotFound { what, .. } if what == "auth session"));
    assert!(
        sessions_b
            .list(&tenant_b, Some(&account_a))
            .await
            .unwrap()
            .is_empty()
    );
    let err = sessions_b
        .revoke(&tenant_b, &id, None, ts(3_000))
        .await
        .unwrap_err();
    assert!(matches!(err, ServiceError::NotFound { .. }));
    // A's session is untouched by B's attempts.
    assert_eq!(
        sessions_a.get(&tenant_a, &id).await.unwrap().state,
        AuthSessionState::Pending
    );

    // No service error prints secret material.
    let secret = "svc-session-secret-do-not-log";
    let text = format!("{not_found}{err}{not_found:?}{err:?}");
    assert!(!text.contains(secret), "error text leaked a secret: {text}");
}
