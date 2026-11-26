//! F040 acceptance (storage): full account deletion — the cascade
//! removes every hard FK child in one transaction and appends an
//! atomic `account.deleted` audit event, observation history survives,
//! and the retention purge sweeps only terminal rows past their cutoff
//! while live and recent rows stay.

mod common;

use common::{ENV, account, gated, tenant_record, ts, unique_entity, unique_tenant};
use fishpond_domain::{
    AccountId, AccountState, AuthSessionId, AuthSessionState, Conversation, ConversationId,
    Message, MessageDirection, MessageId, OperationId, TenantId,
};
use fishpond_storage::{
    CredentialCipher, DeletedAccount, MasterKey, NewAuthSession, NewObservation, NewOperation,
    NewProfile, NewTakeoverGrant, ObservationOutcome, ObservationSource, SecretBox, StorageError,
    Store,
};

fn session_id(tag: &str, n: u32) -> AuthSessionId {
    AuthSessionId::parse(unique_entity("sess", &format!("{tag}-{n}"))).expect("session id")
}

async fn tenant_with_account(
    store: &Store,
    tag: &str,
    state: AccountState,
) -> (TenantId, AccountId) {
    let tenant = unique_tenant(tag);
    store
        .upsert_tenant(&tenant_record(&tenant, tag))
        .await
        .expect("tenant insert");
    let raw = unique_entity("acc", tag);
    let account_id = AccountId::parse(&raw).expect("account id");
    store
        .upsert_account(&account(&raw, &tenant, state, None))
        .await
        .expect("account insert");
    (tenant, account_id)
}

async fn insert_session(
    store: &Store,
    tenant: &TenantId,
    account: &AccountId,
    id: AuthSessionId,
    state: AuthSessionState,
    at: i64,
) {
    store
        .insert_auth_session(&NewAuthSession {
            id,
            tenant: tenant.clone(),
            account_id: account.clone(),
            platform: "mock".to_owned(),
            state,
            created_at: ts(at),
            expires_at: None,
        })
        .await
        .expect("session insert");
}

async fn insert_conversation_with_messages(
    store: &Store,
    tenant: &TenantId,
    account: &AccountId,
    tag: &str,
) -> (ConversationId, usize) {
    let conversation = Conversation {
        id: ConversationId::parse(unique_entity("cnv", tag)).expect("conversation id"),
        account_id: account.clone(),
        tenant_id: tenant.clone(),
        updated_at: ts(2_000),
    };
    store
        .upsert_conversation(&conversation)
        .await
        .expect("conversation insert");
    for n in 0..2 {
        let message = Message::new(
            MessageId::parse(unique_entity("msg", &format!("{tag}-{n}"))).expect("message id"),
            conversation.id.clone(),
            MessageDirection::Outbound,
            format!("hello {n}"),
            ts(2_100 + n),
        )
        .expect("message is valid");
        store
            .insert_message(tenant, &message)
            .await
            .expect("message insert");
    }
    (conversation.id, 2)
}

#[tokio::test]
async fn delete_account_cascades_and_audits() {
    let Some(store) = gated().await else {
        eprintln!("skip: {ENV} is not set (CI has no database)");
        return;
    };
    let (tenant, account_id) = tenant_with_account(&store, "del", AccountState::Disabled).await;

    // Two sessions (one terminal, one live), two messages, one grant on
    // the live session, one lease, one profile — everything with a hard
    // FK to the account.
    insert_session(
        &store,
        &tenant,
        &account_id,
        session_id("del", 1),
        AuthSessionState::Failed,
        1_000,
    )
    .await;
    let live = session_id("del", 2);
    insert_session(
        &store,
        &tenant,
        &account_id,
        live.clone(),
        AuthSessionState::Authorized,
        2_000,
    )
    .await;
    insert_conversation_with_messages(&store, &tenant, &account_id, "del").await;
    store
        .issue_takeover_grant(&NewTakeoverGrant {
            id: &unique_entity("grant", "del"),
            tenant: &tenant,
            account: account_id.as_str(),
            session: live.as_str(),
            operator: "op-del",
            token: &unique_entity("fp_tk", "del"),
            created_at: ts(2_100),
            expires_at: ts(3_000),
        })
        .await
        .expect("grant issue");
    store
        .acquire_lease(
            &tenant,
            &account_id,
            &fishpond_domain::WorkerId::parse(unique_entity("wrk", "del")).expect("worker id"),
            ts(5_000),
            ts(4_000),
        )
        .await
        .expect("lease acquire");
    let cipher = CredentialCipher::new(MasterKey::generate(), 1);
    store
        .upsert_profile(&NewProfile {
            tenant: &tenant,
            id: &unique_entity("prf", "del"),
            account_id: &account_id,
            label: "del profile",
            storage_path: "/tmp/fishpond-del",
            secret: &SecretBox::new("profile-passphrase"),
            cipher: &cipher,
            at: ts(2_500),
        })
        .await
        .expect("profile insert");
    store
        .record_observation(&NewObservation {
            id: &unique_entity("obs", "del"),
            tenant: &tenant,
            account: &account_id,
            source: ObservationSource::Reconcile,
            observed: "ONLINE",
            outcome: ObservationOutcome::Unchanged,
            note: Some("audit probe before delete"),
            operator: None,
            at: ts(2_600),
        })
        .await
        .expect("observation insert");

    let summary = store.delete_account(&tenant, &account_id, ts(9_000)).await;
    let summary: DeletedAccount = summary.expect("account deleted");
    assert_eq!(summary.conversations, 1);
    assert_eq!(summary.messages, 2);
    assert_eq!(summary.sessions, 2);
    assert_eq!(summary.grants, 1);
    assert_eq!(summary.leases, 1);
    assert_eq!(summary.profiles, 1);

    // The account itself is gone.
    let gone = store.get_account(&tenant, &account_id).await.unwrap_err();
    assert!(matches!(gone, StorageError::NotFound { .. }), "{gone}");

    // Observation history survives the deletion (migration 0014).
    let history = store
        .list_observations(&tenant, &account_id)
        .await
        .expect("observations readable after delete");
    assert_eq!(history.len(), 1);

    // The audit event is in the log with the cascade summary.
    let events = store
        .list_events(&tenant, 0, 10)
        .await
        .expect("events after delete");
    let deleted = events
        .iter()
        .find(|event| event.event_type == "account.deleted")
        .expect("account.deleted event appended");
    assert_eq!(deleted.account_id, Some(account_id.clone()));
    let payload = deleted.payload.as_str();
    assert!(payload.contains("\"sessions\":2"), "{payload}");
    assert!(payload.contains("\"messages\":2"), "{payload}");
    assert!(payload.contains("\"profiles\":1"), "{payload}");
}

#[tokio::test]
async fn delete_account_is_tenant_scoped() {
    let Some(store) = gated().await else {
        eprintln!("skip: {ENV} is not set (CI has no database)");
        return;
    };
    // Account ids are globally unique (accounts.id is the primary key),
    // so each tenant holds a distinct id; the isolation property under
    // test is that a delete scoped to one tenant never removes another
    // tenant's row.
    let own_a = unique_entity("acc", "dela");
    let own_b = unique_entity("acc", "delb");
    let tenant_a = unique_tenant("dela");
    let tenant_b = unique_tenant("delb");
    for (tenant, tag, raw) in [(&tenant_a, "dela", &own_a), (&tenant_b, "delb", &own_b)] {
        store
            .upsert_tenant(&tenant_record(tenant, tag))
            .await
            .expect("tenant insert");
        store
            .upsert_account(&account(raw, tenant, AccountState::Offline, None))
            .await
            .expect("account insert");
    }
    let id_a = AccountId::parse(&own_a).expect("id");
    let id_b = AccountId::parse(&own_b).expect("id");

    // Tenant A cannot even read tenant B's row (RLS), let alone delete it.
    let foreign = store.get_account(&tenant_a, &id_b).await.unwrap_err();
    assert!(
        matches!(foreign, StorageError::NotFound { .. }),
        "{foreign}"
    );

    // Deleting tenant A's account must not touch tenant B's row.
    store
        .delete_account(&tenant_a, &id_a, ts(9_000))
        .await
        .expect("tenant A account deleted");
    let kept = store
        .get_account(&tenant_b, &id_b)
        .await
        .expect("tenant B account untouched");
    assert_eq!(kept.id.as_str(), own_b);

    // A tenant that does not own the id gets NotFound, never a delete —
    // even though the id still exists under tenant A's old scope.
    let stranger = unique_tenant("delc");
    store
        .upsert_tenant(&tenant_record(&stranger, "delc"))
        .await
        .expect("tenant insert");
    let wrong = store
        .delete_account(&stranger, &id_a, ts(9_000))
        .await
        .unwrap_err();
    assert!(matches!(wrong, StorageError::NotFound { .. }), "{wrong}");
}

#[tokio::test]
async fn delete_missing_account_is_not_found() {
    let Some(store) = gated().await else {
        eprintln!("skip: {ENV} is not set (CI has no database)");
        return;
    };
    let tenant = unique_tenant("delmiss");
    store
        .upsert_tenant(&tenant_record(&tenant, "delmiss"))
        .await
        .expect("tenant insert");
    let missing = AccountId::parse(unique_entity("acc", "delmiss")).expect("id");
    let error = store
        .delete_account(&tenant, &missing, ts(9_000))
        .await
        .unwrap_err();
    assert!(matches!(error, StorageError::NotFound { .. }), "{error}");
}

/// Ages rows past the cutoff with tenant-bound SQL — unbound sessions
/// fail closed under row-level security, so the update must run inside
/// the same binding the repositories use (the purge's cutoff predicate
/// is the thing under test; the closed-row state machines have their
/// own suites).
async fn age(store: &Store, tenant: &TenantId, sql: &str) {
    let mut tx = store.begin_tenant(tenant).await.expect("tenant-bound tx");
    sqlx::query(sql)
        .bind(tenant.as_str())
        .execute(&mut *tx)
        .await
        .expect("age update");
    tx.commit().await.expect("age commit");
}

#[tokio::test]
async fn purge_sweeps_only_terminal_rows_past_the_cutoff() {
    let Some(store) = gated().await else {
        eprintln!("skip: {ENV} is not set (CI has no database)");
        return;
    };
    let (tenant, account_id) = tenant_with_account(&store, "purge", AccountState::Online).await;

    // Old terminal session + its expired grant: both purge.
    let old = session_id("purge", 1);
    insert_session(
        &store,
        &tenant,
        &account_id,
        old.clone(),
        AuthSessionState::Failed,
        1_000,
    )
    .await;
    store
        .issue_takeover_grant(&NewTakeoverGrant {
            id: &unique_entity("grant", "purge-old"),
            tenant: &tenant,
            account: account_id.as_str(),
            session: old.as_str(),
            operator: "op-purge",
            token: &unique_entity("fp_tk", "purge-old"),
            created_at: ts(1_500),
            expires_at: ts(1_600),
        })
        .await
        .expect("grant issue");

    // Recent live session: stays. (The id is captured once —
    // unique_entity mints a fresh id on every call.)
    let fresh = session_id("purge", 2);
    insert_session(
        &store,
        &tenant,
        &account_id,
        fresh.clone(),
        AuthSessionState::Authorized,
        8_000,
    )
    .await;

    // One old SUCCEEDED operation, one fresh PENDING operation.
    let old_op = OperationId::parse(unique_entity("op", "purge-old")).expect("operation id");
    store
        .create_operation(&NewOperation {
            id: &old_op,
            tenant: &tenant,
            kind: "send_message",
            idempotency_key: &unique_entity("idem", "purge-old"),
            at: ts(1_000),
        })
        .await
        .expect("operation create");
    age(
        &store,
        &tenant,
        "UPDATE operations SET state = 'SUCCEEDED', updated_at = 2000 \
         WHERE tenant_id = $1 AND state = 'PENDING'",
    )
    .await;
    let fresh_op = OperationId::parse(unique_entity("op", "purge-fresh")).expect("operation id");
    store
        .create_operation(&NewOperation {
            id: &fresh_op,
            tenant: &tenant,
            kind: "send_message",
            idempotency_key: &unique_entity("idem", "purge-fresh"),
            at: ts(8_500),
        })
        .await
        .expect("operation create");

    // One old delivered webhook delivery, one fresh pending one.
    store
        .insert_delivery(&fishpond_storage::NewDelivery {
            id: &unique_entity("dlv", "purge-old"),
            tenant: &tenant,
            subscription_id: "sub-purge",
            event_id: &fishpond_domain::EventId::parse(unique_entity("evt", "purge-old"))
                .expect("event id"),
            event_seq: 1,
            body: "{}",
            at: ts(1_000),
        })
        .await
        .expect("delivery insert");
    age(
        &store,
        &tenant,
        "UPDATE webhook_deliveries SET status = 'delivered', updated_at = 2000 \
         WHERE tenant_id = $1 AND status = 'pending'",
    )
    .await;

    // An equally stale session in another tenant: the sweep is
    // tenant-scoped, so it must survive tenant A's sweep untouched.
    let (other, other_account) = tenant_with_account(&store, "purgeb", AccountState::Online).await;
    let other_session = session_id("purgeb", 1);
    insert_session(
        &store,
        &other,
        &other_account,
        other_session.clone(),
        AuthSessionState::Failed,
        1_000,
    )
    .await;

    // An empty sweep still reports zero; the cutoff keeps everything
    // fresh (the old session's grant is live at this cutoff, so the
    // session is guarded too).
    let early = store
        .purge_expired(&tenant, ts(1_100))
        .await
        .expect("early purge");
    assert_eq!(early.sessions, 0);
    assert_eq!(early.grants, 0);
    assert_eq!(early.operations, 0);
    assert_eq!(early.deliveries, 0);

    let report = store
        .purge_expired(&tenant, ts(5_000))
        .await
        .expect("purge");
    assert_eq!(report.sessions, 1, "old failed session purged");
    assert_eq!(report.grants, 1, "expired grant purged");
    assert_eq!(report.operations, 1, "old succeeded operation purged");
    assert_eq!(report.deliveries, 1, "old delivered delivery purged");

    // The other tenant's stale row is untouched by tenant A's sweep.
    store
        .get_auth_session(&other, &other_session)
        .await
        .expect("other tenant's stale session kept");

    // Live and recent rows survive the sweep.
    store
        .get_auth_session(&tenant, &fresh)
        .await
        .expect("live session kept");
    store
        .get_operation(&tenant, &fresh_op)
        .await
        .expect("fresh operation kept");

    // Idempotent: a second sweep finds nothing left past the cutoff.
    let again = store
        .purge_expired(&tenant, ts(5_000))
        .await
        .expect("second purge");
    assert_eq!(again.sessions, 0);
    assert_eq!(again.grants, 0);
    assert_eq!(again.operations, 0);
    assert_eq!(again.deliveries, 0);
}
