//! F037 acceptance (profile secure migration): `ProfileManager::migrate`
//! moves a profile to a new on-disk path without ever holding an
//! unverified copy as the live state. Under the exclusive lock it
//! snapshots the source tree with the integrity manifest, materializes
//! the snapshot at the destination, re-verifies every copied byte,
//! swaps the row, keeps the snapshot as the rollback artifact, and only
//! then removes the old tree. The sealed passphrase envelope is not
//! rewritten (no re-seal, no cipher needed), destroyed tombstones and
//! same-path no-ops are refused, and the whole operation holds the
//! profile's exclusive lock — a live engine holder blocks it. Same
//! skip contract as the other worker tests: without a database the
//! file is a no-op.

mod common;

use std::path::{Path, PathBuf};

use std::time::{SystemTime, UNIX_EPOCH};

use common::{account, gated, tenant_record, ts, unique_entity, unique_tenant};
use fishpond_domain::{AccountState, TenantId, WorkerId};
use fishpond_storage::{CredentialCipher, MasterKey, NewProfile, SecretBox, Store};
use fishpond_worker::{MigrationTarget, ProfileError, ProfileManager};

fn unique_version() -> i32 {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock is after the epoch")
        .as_nanos();
    nanos as i32
}

async fn cipher_for(store: &Store) -> CredentialCipher {
    let version = unique_version();
    store
        .register_master_key(version, ts(500))
        .await
        .expect("master key registration");
    CredentialCipher::new(MasterKey::generate(), version)
}

async fn tenant_for(store: &Store, tag: &str) -> TenantId {
    let tenant = unique_tenant(tag);
    store
        .upsert_tenant(&tenant_record(&tenant, tag))
        .await
        .expect("tenant insert");
    tenant
}

/// A registered profile whose on-disk directory exists with two files
/// (one nested), returning (profile_id, dir).
async fn register_with_dir(
    store: &Store,
    tenant: &TenantId,
    tag: &str,
) -> (String, PathBuf, CredentialCipher) {
    let id = unique_entity("prof", tag);
    let dir = std::env::temp_dir().join(unique_entity("profdir", tag));
    std::fs::create_dir_all(dir.join("nested")).expect("profile dir");
    std::fs::write(dir.join("Cookies"), "cookie-jar-v1").expect("cookie file");
    std::fs::write(dir.join("nested").join("log"), "session-log").expect("log file");

    let account_id = unique_entity("acc", tag);
    store
        .upsert_account(&account(&account_id, tenant, AccountState::Online, None))
        .await
        .expect("account insert");
    let cipher = cipher_for(store).await;
    store
        .upsert_profile(&NewProfile {
            tenant,
            id: &id,
            account_id: &fishpond_domain::AccountId::parse(account_id).expect("account id"),
            label: "migrate test profile",
            storage_path: dir.to_str().expect("utf-8 temp path"),
            secret: &SecretBox::new("passphrase"),
            cipher: &cipher,
            at: ts(3_000),
        })
        .await
        .expect("profile insert");
    (id, dir, cipher)
}

fn holder(tag: &str) -> WorkerId {
    WorkerId::parse(unique_entity("wkr", tag)).expect("worker id")
}

fn root_for(tag: &str) -> PathBuf {
    std::env::temp_dir().join(unique_entity("backuproot", tag))
}

fn target<'a>(storage_path: &'a str, backup_root: &'a Path) -> MigrationTarget<'a> {
    MigrationTarget {
        storage_path,
        backup_root,
    }
}

#[tokio::test]
async fn migrate_moves_a_verified_profile_and_keeps_the_snapshot() {
    let Some(store) = gated().await else {
        eprintln!("skip: no database");
        return;
    };
    let tenant = tenant_for(&store, "mig-move").await;
    let manager = ProfileManager::new(store.clone());
    let (id, old_dir, cipher) = register_with_dir(&store, &tenant, "move").await;
    let root = root_for("move");
    let alice = holder("alice");
    let new_dir = std::env::temp_dir().join(unique_entity("profdest", "move"));
    let new_path = new_dir.to_str().expect("utf-8 temp path");

    let report = manager
        .migrate(
            &tenant,
            &id,
            &alice,
            &target(new_path, &root),
            60_000,
            ts(4_000),
        )
        .await
        .expect("migrate");
    assert_eq!(
        report.from,
        old_dir.to_str().expect("utf-8"),
        "the report names the old path"
    );
    assert_eq!(report.to, new_path);
    assert_eq!(report.file_count, 2, "Cookies + nested/log");
    assert_eq!(
        report.total_bytes,
        ("cookie-jar-v1".len() + "session-log".len()) as u64
    );

    // The row points at the new path, back in active state, with the
    // verified snapshot recorded as the rollback artifact.
    let row = store
        .get_profile(&tenant, &id)
        .await
        .expect("get")
        .expect("profile exists");
    assert_eq!(row.storage_path, new_path);
    assert_eq!(row.state, fishpond_storage::ProfileState::Active);
    assert_eq!(row.backup_digest.as_deref(), Some(report.digest.as_str()));
    assert_eq!(row.last_backup_at, Some(ts(4_000)));

    // The contents live at the destination; the old tree is gone; the
    // snapshot survives under the backup root.
    assert_eq!(
        std::fs::read_to_string(new_dir.join("Cookies")).expect("moved cookie"),
        "cookie-jar-v1",
        "the profile contents are at the new path"
    );
    assert_eq!(
        std::fs::read_to_string(new_dir.join("nested").join("log")).expect("moved log"),
        "session-log"
    );
    assert!(
        !old_dir.exists(),
        "the old tree was removed: {}",
        old_dir.display()
    );
    assert!(report.snapshot_dir.is_dir(), "the snapshot is kept");
    let manifest = std::fs::read_to_string(report.snapshot_dir.join("manifest.json"))
        .expect("snapshot manifest readable");
    assert!(manifest.contains(&report.digest), "{manifest}");

    // The sealed envelope was never rewritten: the passphrase still
    // opens without any re-seal.
    let secret = store
        .open_profile_secret(&tenant, &id, &cipher)
        .await
        .expect("envelope untouched");
    assert_eq!(secret.expose(), "passphrase");

    // Rollback story: restore() finds the migration snapshot and lands
    // the profile at its (new) registered path.
    manager
        .restore(&tenant, &id, &alice, &root, 60_000, ts(4_200))
        .await
        .expect("restore after migrate");
    assert_eq!(
        std::fs::read_to_string(new_dir.join("Cookies")).expect("restored at new path"),
        "cookie-jar-v1",
        "restore targets the new registered path"
    );
}

#[tokio::test]
async fn migrate_refuses_same_path_destroyed_and_missing_trees() {
    let Some(store) = gated().await else {
        eprintln!("skip: no database");
        return;
    };
    let tenant = tenant_for(&store, "mig-refuse").await;
    let manager = ProfileManager::new(store.clone());
    let (id, dir, _cipher) = register_with_dir(&store, &tenant, "refuse").await;
    let root = root_for("refuse");
    let alice = holder("alice");

    // Same path: nothing to move, refused before any disk work.
    let same = manager
        .migrate(
            &tenant,
            &id,
            &alice,
            &target(dir.to_str().expect("utf-8"), &root),
            60_000,
            ts(4_000),
        )
        .await
        .expect_err("same path");
    assert!(
        matches!(&same, ProfileError::SamePath { .. }),
        "expected SamePath, got {same:?}"
    );

    // Missing source directory: named, nothing created.
    let ghost_id = unique_entity("prof", "ghost");
    let ghost_dir = std::env::temp_dir().join(unique_entity("never-created", "refuse"));
    let account_id = unique_entity("acc", "ghost");
    store
        .upsert_account(&account(&account_id, &tenant, AccountState::Online, None))
        .await
        .expect("account insert");
    store
        .upsert_profile(&NewProfile {
            tenant: &tenant,
            id: &ghost_id,
            account_id: &fishpond_domain::AccountId::parse(account_id).expect("account id"),
            label: "ghost",
            storage_path: ghost_dir.to_str().expect("utf-8"),
            secret: &SecretBox::new("passphrase"),
            cipher: &cipher_for(&store).await,
            at: ts(3_500),
        })
        .await
        .expect("profile insert");
    let destination = std::env::temp_dir().join(unique_entity("profdest", "ghost"));
    let missing = manager
        .migrate(
            &tenant,
            &ghost_id,
            &alice,
            &target(destination.to_str().expect("utf-8"), &root),
            60_000,
            ts(4_100),
        )
        .await
        .expect_err("missing tree");
    assert!(
        matches!(&missing, ProfileError::ProfileDirMissing { path } if Path::new(path) == ghost_dir),
        "expected ProfileDirMissing, got {missing:?}"
    );
    assert!(!destination.exists(), "no destination was created");

    // Destroyed tombstone: migration refused, row still names the old
    // path, destination never created.
    let (dead_id, dead_dir, _cipher) = register_with_dir(&store, &tenant, "dead").await;
    manager
        .destroy(&tenant, &dead_id, &alice, 60_000, ts(4_200))
        .await
        .expect("destroy");
    let dead_dest = std::env::temp_dir().join(unique_entity("profdest", "dead"));
    let dead = manager
        .migrate(
            &tenant,
            &dead_id,
            &alice,
            &target(dead_dest.to_str().expect("utf-8"), &root),
            60_000,
            ts(4_300),
        )
        .await
        .expect_err("destroyed profile");
    assert!(
        dead.to_string().contains("destroyed"),
        "the refusal names the tombstone: {dead}"
    );
    let row = store
        .get_profile(&tenant, &dead_id)
        .await
        .expect("get")
        .expect("tombstone stays");
    assert_eq!(row.state, fishpond_storage::ProfileState::Destroyed);
    assert!(!dead_dest.exists(), "no destination was created");
    let _ = dead_dir; // destroyed profiles have no tree left by design
}

#[tokio::test]
async fn migrate_holds_the_lock_and_is_tenant_scoped() {
    let Some(store) = gated().await else {
        eprintln!("skip: no database");
        return;
    };
    let tenant_a = tenant_for(&store, "mig-lock-a").await;
    let tenant_b = tenant_for(&store, "mig-lock-b").await;
    let manager = ProfileManager::new(store.clone());
    let (id, _dir, _cipher) = register_with_dir(&store, &tenant_a, "lock").await;
    let root = root_for("lock");
    let destination = std::env::temp_dir().join(unique_entity("profdest", "lock"));
    let destination_path = destination.to_str().expect("utf-8");

    // A live foreign holder blocks the whole migration.
    let bob = holder("bob");
    store
        .acquire_profile_lock(&tenant_a, &id, &bob, ts(10_000), ts(1_000))
        .await
        .expect("bob holds");
    let alice = holder("alice");
    let blocked = manager
        .migrate(
            &tenant_a,
            &id,
            &alice,
            &target(destination_path, &root),
            60_000,
            ts(4_000),
        )
        .await
        .expect_err("live foreign holder");
    assert!(
        blocked.to_string().contains(bob.as_str()),
        "the conflict names the holder: {blocked}"
    );
    // Nothing was moved while the lock was held elsewhere.
    let row = store
        .get_profile(&tenant_a, &id)
        .await
        .expect("get")
        .expect("profile exists");
    assert!(!destination.exists(), "no destination was created");
    assert_ne!(row.storage_path, destination_path);
    store
        .release_profile_lock(&tenant_a, &id, &bob)
        .await
        .expect("bob releases");

    // Tenant B cannot migrate A's profile.
    let foreign = manager
        .migrate(
            &tenant_b,
            &id,
            &holder("carol"),
            &target(destination_path, &root),
            60_000,
            ts(4_100),
        )
        .await
        .expect_err("foreign tenant");
    assert!(
        foreign.to_string().contains("not found"),
        "the foreign lookup misses: {foreign}"
    );

    // Failed operations left the lock free: the next holder acquires
    // at generation 1.
    let lock = store
        .acquire_profile_lock(&tenant_a, &id, &bob, ts(20_000), ts(4_200))
        .await
        .expect("lock free after failed operations");
    assert_eq!(lock.generation, 1, "no steal happened — holders released");
}
