//! F022 acceptance (storage side): the profile registry and its
//! exclusive lock.
//!
//! Profiles seal their passphrase before the database (F008 envelope),
//! walk active → backed_up → destroyed with bookkeeping, and the lock
//! follows the account-lease CAS discipline: one live holder,
//! generation fencing, expiry as the steal path, destroyed profiles
//! never lockable. Both tables are tenant-isolated.

mod common;

use common::{account, gated, tenant_record, ts, unique_entity, unique_tenant, unique_version};
use fishpond_domain::{AccountState, TenantId, WorkerId};
use fishpond_storage::{
    CredentialCipher, MasterKey, NewProfile, ProfileState, ProfileState as State, SecretBox, Store,
};
use sqlx::Row;

async fn tenant_for(store: &Store, tag: &str) -> TenantId {
    let tenant = unique_tenant(tag);
    store
        .upsert_tenant(&tenant_record(&tenant, tag))
        .await
        .expect("tenant insert");
    tenant
}

async fn cipher_for(store: &Store) -> CredentialCipher {
    let version = unique_version();
    store
        .register_master_key(version, ts(500))
        .await
        .expect("master key registration");
    CredentialCipher::new(MasterKey::generate(), version)
}

async fn register(
    store: &Store,
    tenant: &TenantId,
    id: &str,
    path: &str,
    secret: &str,
    cipher: &CredentialCipher,
) -> fishpond_storage::ProfileRecord {
    // The profile id is already unique-stamped; prefix it instead of
    // stamping again (double stamps overflow the id length cap).
    let account_id = format!("acc-{id}");
    store
        .upsert_account(&account(&account_id, tenant, AccountState::Online, None))
        .await
        .expect("account insert");
    store
        .upsert_profile(&NewProfile {
            tenant,
            id,
            account_id: &fishpond_domain::AccountId::parse(account_id.clone()).expect("account id"),
            label: "test profile",
            storage_path: path,
            secret: &SecretBox::new(secret),
            cipher,
            at: ts(3_000),
        })
        .await
        .expect("profile insert")
}

fn holder(tag: &str) -> WorkerId {
    WorkerId::parse(unique_entity("wkr", tag)).expect("worker id")
}

#[tokio::test]
async fn profile_registry_seals_rotates_and_walks_states() {
    let Some(store) = gated().await else {
        eprintln!("skip: no database");
        return;
    };
    let tenant = tenant_for(&store, "prof-life").await;
    let cipher = cipher_for(&store).await;
    let id = unique_entity("prof", "life");

    // Registration is `active` and the passphrase round-trips only
    // through the cipher.
    let profile = register(&store, &tenant, &id, "/tmp/fishpond/p1", "pass-1", &cipher).await;
    assert_eq!(profile.state, ProfileState::Active);
    assert_eq!(profile.label, "test profile");
    assert!(profile.backup_digest.is_none());
    let opened = store
        .open_profile_secret(&tenant, &id, &cipher)
        .await
        .expect("open sealed passphrase");
    assert_eq!(opened.expose(), "pass-1");

    // Rotation: same id re-registered rotates label/path/secret but
    // keeps state and backup bookkeeping.
    let rotated = register(&store, &tenant, &id, "/tmp/fishpond/p2", "pass-2", &cipher).await;
    assert_eq!(rotated.storage_path, "/tmp/fishpond/p2");
    assert_eq!(rotated.state, ProfileState::Active);
    assert_ne!(rotated.secret_fingerprint, profile.secret_fingerprint);
    let opened = store
        .open_profile_secret(&tenant, &id, &cipher)
        .await
        .expect("open rotated passphrase");
    assert_eq!(opened.expose(), "pass-2");

    // Backup bookkeeping: digest + instant + state, then restore puts
    // it back to active with the digest kept.
    let backed = store
        .record_backup(&tenant, &id, "sha256:abc", ts(4_000))
        .await
        .expect("record backup");
    assert_eq!(backed.state, State::BackedUp);
    assert_eq!(backed.backup_digest.as_deref(), Some("sha256:abc"));
    assert_eq!(backed.last_backup_at, Some(ts(4_000)));
    let restored = store
        .mark_profile_restored(&tenant, &id, ts(4_100))
        .await
        .expect("mark restored");
    assert_eq!(restored.state, ProfileState::Active);
    assert_eq!(restored.backup_digest.as_deref(), Some("sha256:abc"));

    // Destruction is a tombstone: state flips, later mutations are
    // refused, but the row stays listed for audit.
    let destroyed = store
        .destroy_profile(&tenant, &id, ts(5_000))
        .await
        .expect("destroy");
    assert_eq!(destroyed.state, ProfileState::Destroyed);
    let mutation = store
        .record_backup(&tenant, &id, "sha256:def", ts(5_100))
        .await;
    assert!(
        matches!(&mutation, Err(fishpond_storage::StorageError::NotFound { what, .. }) if *what == "profile"),
        "destroyed profiles refuse mutations: {mutation:?}"
    );
    assert_eq!(store.list_profiles(&tenant).await.expect("list").len(), 1);
}

#[tokio::test]
async fn profile_lock_is_exclusive_with_lease_fencing() {
    let Some(store) = gated().await else {
        eprintln!("skip: no database");
        return;
    };
    let tenant = tenant_for(&store, "prof-lock").await;
    let cipher = cipher_for(&store).await;
    let id = unique_entity("prof", "lock");
    register(&store, &tenant, &id, "/tmp/fishpond/lock", "pass", &cipher).await;

    let alice = holder("alice");
    let bob = holder("bob");

    // First holder: generation 1; a live foreign acquire is a
    // conflict naming holder and generation.
    let lock = store
        .acquire_profile_lock(&tenant, &id, &alice, ts(10_000), ts(1_000))
        .await
        .expect("first acquire");
    assert_eq!(lock.generation, 1);
    let foreign = store
        .acquire_profile_lock(&tenant, &id, &bob, ts(10_000), ts(2_000))
        .await
        .expect_err("live foreign acquire");
    let text = foreign.to_string();
    assert!(
        text.contains(alice.as_str()) && text.contains("generation 1"),
        "{text}"
    );

    // Renewal extends the term without touching the generation.
    let renewed = store
        .renew_profile_lock(&tenant, &id, &alice, ts(11_000), ts(2_500))
        .await
        .expect("renew own lock");
    assert_eq!(renewed.generation, 1);
    assert_eq!(renewed.expires_at, ts(11_000));

    // An expired term is never renewed in place — re-acquire; the
    // other worker's steal bumps the generation (fencing token).
    let self_renew = store
        .renew_profile_lock(&tenant, &id, &alice, ts(12_000), ts(12_500))
        .await
        .expect_err("expired self renew");
    assert!(
        self_renew.to_string().contains("re-acquire"),
        "{self_renew}"
    );
    let stolen = store
        .acquire_profile_lock(&tenant, &id, &bob, ts(20_000), ts(12_600))
        .await
        .expect("steal expired lock");
    assert_eq!(stolen.generation, 2);
    assert_eq!(stolen.holder, bob.as_str());

    // Release removes the row; a fresh acquire starts at generation 1
    // (the documented release-then-reacquire reset, as F012).
    assert!(
        store
            .release_profile_lock(&tenant, &id, &bob)
            .await
            .expect("release")
    );
    assert!(
        store
            .get_profile_lock(&tenant, &id)
            .await
            .expect("get lock")
            .is_none()
    );
    let fresh = store
        .acquire_profile_lock(&tenant, &id, &alice, ts(30_000), ts(13_000))
        .await
        .expect("acquire after release");
    assert_eq!(fresh.generation, 1);

    // Destroy closes the lock bookkeeping and the tombstone refuses
    // any further lock.
    store
        .destroy_profile(&tenant, &id, ts(14_000))
        .await
        .expect("destroy");
    let tombstone = store
        .acquire_profile_lock(&tenant, &id, &alice, ts(40_000), ts(14_100))
        .await
        .expect_err("destroyed acquire");
    assert!(tombstone.to_string().contains("destroyed"), "{tombstone}");
    // Destroy also removed the live lock row.
    assert!(
        store
            .get_profile_lock(&tenant, &id)
            .await
            .expect("get lock")
            .is_none()
    );
}

#[tokio::test]
async fn profiles_and_locks_are_tenant_isolated() {
    let Some(store) = gated().await else {
        eprintln!("skip: no database");
        return;
    };
    let tenant_a = tenant_for(&store, "prof-iso-a").await;
    let tenant_b = tenant_for(&store, "prof-iso-b").await;
    let cipher = cipher_for(&store).await;
    let id = unique_entity("prof", "iso");
    register(
        &store,
        &tenant_a,
        &id,
        "/tmp/fishpond/iso",
        "secret-a",
        &cipher,
    )
    .await;
    let alice = holder("a");
    store
        .acquire_profile_lock(&tenant_a, &id, &alice, ts(10_000), ts(1_000))
        .await
        .expect("tenant a lock");

    // Tenant B cannot see the profile, open its secret, or lock it.
    assert!(
        store
            .get_profile(&tenant_b, &id)
            .await
            .expect("foreign get")
            .is_none()
    );
    assert!(
        store
            .list_profiles(&tenant_b)
            .await
            .expect("list")
            .is_empty()
    );
    assert!(
        store
            .open_profile_secret(&tenant_b, &id, &cipher)
            .await
            .is_err()
    );
    let bob = holder("b");
    let foreign = store
        .acquire_profile_lock(&tenant_b, &id, &bob, ts(10_000), ts(2_000))
        .await
        .expect_err("foreign lock");
    assert!(
        foreign.to_string().contains("not found")
            || matches!(foreign, fishpond_storage::StorageError::NotFound { .. }),
        "{foreign}"
    );

    // A bare pool (no tenant bound) sees zero rows in both tables.
    let bare = store.pool();
    let profiles = sqlx::query("SELECT COUNT(*) AS n FROM profiles")
        .fetch_one(bare)
        .await
        .expect("bare profiles count");
    let locks = sqlx::query("SELECT COUNT(*) AS n FROM profile_locks")
        .fetch_one(bare)
        .await
        .expect("bare locks count");
    assert_eq!(profiles.try_get::<i64, _>("n").expect("n"), 0);
    assert_eq!(locks.try_get::<i64, _>("n").expect("n"), 0);
}
