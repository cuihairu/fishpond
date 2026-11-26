//! F022 acceptance (worker side): `ProfileManager` — the on-disk
//! backup/restore/destroy lifecycle under the exclusive lock (T11) and
//! its integrity rules (T12).
//!
//! Backups snapshot the profile directory with a per-file SHA-256
//! manifest; restores verify every file against the manifest before
//! touching the live directory (drift is `Tampered`, naming the file);
//! destroy wipes the tree and tombstones the row. Every operation
//! takes the profile lock and releases it on failure — a failed backup
//! never wedges the profile. Same skip contract as the storage tests:
//! without a database the file is a no-op.

mod common;

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use common::{account, gated, tenant_record, ts, unique_entity, unique_tenant};
use fishpond_domain::{AccountState, TenantId, WorkerId};
use fishpond_storage::{CredentialCipher, MasterKey, NewProfile, SecretBox, Store};
use fishpond_worker::{ProfileError, ProfileManager};

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
async fn register_with_dir(store: &Store, tenant: &TenantId, tag: &str) -> (String, PathBuf) {
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
    store
        .upsert_profile(&NewProfile {
            tenant,
            id: &id,
            account_id: &fishpond_domain::AccountId::parse(account_id).expect("account id"),
            label: "worker test profile",
            storage_path: dir.to_str().expect("utf-8 temp path"),
            secret: &SecretBox::new("passphrase"),
            cipher: &cipher_for(store).await,
            at: ts(3_000),
        })
        .await
        .expect("profile insert");
    (id, dir)
}

fn holder(tag: &str) -> WorkerId {
    WorkerId::parse(unique_entity("wkr", tag)).expect("worker id")
}

fn root_for(tag: &str) -> PathBuf {
    std::env::temp_dir().join(unique_entity("backuproot", tag))
}

fn restore_error(err: ProfileError) -> String {
    err.to_string()
}

#[tokio::test]
async fn backup_restore_roundtrip_detects_tampering() {
    let Some(store) = gated().await else {
        eprintln!("skip: no database");
        return;
    };
    let tenant = tenant_for(&store, "pm-round").await;
    let manager = ProfileManager::new(store.clone());
    let (id, dir) = register_with_dir(&store, &tenant, "round").await;
    let root = root_for("round");
    let alice = holder("alice");
    let term = 60_000i64;

    // Backup: every file captured, digest recorded, state flips.
    let report = manager
        .backup(&tenant, &id, &alice, &root, term, ts(4_000))
        .await
        .expect("first backup");
    assert_eq!(report.file_count, 2, "Cookies + nested/log");
    assert_eq!(
        report.total_bytes,
        ("cookie-jar-v1".len() + "session-log".len()) as u64
    );
    let row = store
        .get_profile(&tenant, &id)
        .await
        .expect("get")
        .expect("profile exists");
    assert_eq!(row.state, fishpond_storage::ProfileState::BackedUp);
    assert_eq!(row.backup_digest.as_deref(), Some(report.digest.as_str()));
    assert_eq!(row.last_backup_at, Some(ts(4_000)));

    // The manifest hashes exactly the captured files.
    let manifest: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(report.snapshot_dir.join("manifest.json"))
            .expect("manifest readable"),
    )
    .expect("manifest valid json");
    assert_eq!(manifest["profile"], id.as_str());
    assert_eq!(manifest["files"].as_array().expect("files").len(), 2);

    // Mutate the live directory, then restore puts the snapshot back.
    std::fs::write(dir.join("Cookies"), "mutated-after-backup").expect("mutate");
    manager
        .restore(&tenant, &id, &alice, &root, term, ts(4_100))
        .await
        .expect("restore");
    assert_eq!(
        std::fs::read_to_string(dir.join("Cookies")).expect("restored cookie"),
        "cookie-jar-v1",
        "restore replaced the live contents"
    );
    let row = store
        .get_profile(&tenant, &id)
        .await
        .expect("get")
        .expect("profile exists");
    assert_eq!(row.state, fishpond_storage::ProfileState::Active);
    assert_eq!(row.backup_digest.as_deref(), Some(report.digest.as_str()));

    // A second backup lands as its own snapshot and restores newest-first.
    std::fs::write(dir.join("Cookies"), "cookie-jar-v2").expect("second content");
    let second = manager
        .backup(&tenant, &id, &alice, &root, term, ts(4_500))
        .await
        .expect("second backup");
    assert_ne!(second.snapshot_dir, report.snapshot_dir);
    assert_ne!(second.digest, report.digest);
    manager
        .restore(&tenant, &id, &alice, &root, term, ts(4_600))
        .await
        .expect("restore latest");
    assert_eq!(
        std::fs::read_to_string(dir.join("Cookies")).expect("restored v2"),
        "cookie-jar-v2",
        "the latest snapshot wins"
    );

    // Tamper with a snapshot file: restore refuses, naming the file.
    let snap_file = second.snapshot_dir.join("nested").join("log");
    std::fs::write(&snap_file, "injected-payload").expect("tamper");
    let tampered = manager
        .restore(&tenant, &id, &alice, &root, term, ts(4_700))
        .await
        .expect_err("tampered snapshot");
    match &tampered {
        ProfileError::Tampered { path } => assert_eq!(path, "nested/log", "{tampered:?}"),
        other => panic!("expected Tampered, got {other:?}"),
    }
    // The live directory was not touched by the refused restore.
    assert_eq!(
        std::fs::read_to_string(dir.join("Cookies")).expect("live untouched"),
        "cookie-jar-v2"
    );
    std::fs::write(&snap_file, "session-log").expect("un-tamper");

    // Tamper with the manifest digest itself: refused, naming the manifest.
    let manifest_path = second.snapshot_dir.join("manifest.json");
    let mut manifest: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&manifest_path).expect("manifest readable"))
            .expect("manifest valid json");
    manifest["digest"] = serde_json::Value::String("0".repeat(64));
    std::fs::write(&manifest_path, manifest.to_string()).expect("rewrite manifest");
    let tampered = manager
        .restore(&tenant, &id, &alice, &root, term, ts(4_800))
        .await
        .expect_err("tampered manifest");
    assert!(
        matches!(&tampered, ProfileError::Tampered { path } if path == "manifest.json"),
        "{tampered:?}"
    );
    std::fs::write(&manifest_path, {
        let mut original = manifest.clone();
        original["digest"] = serde_json::Value::String(second.digest.clone());
        original.to_string()
    })
    .expect("restore manifest");

    // A profile with no snapshots refuses to restore.
    let (fresh_id, _) = register_with_dir(&store, &tenant, "nobackup").await;
    let empty_root = root_for("nobackup");
    let no_backup = manager
        .restore(&tenant, &fresh_id, &alice, &empty_root, term, ts(5_000))
        .await
        .expect_err("no snapshot");
    assert!(matches!(no_backup, ProfileError::NoBackup), "{no_backup:?}");

    // A missing profile directory refuses to backup, naming the path.
    let missing_id = unique_entity("prof", "missing");
    let missing_dir = std::env::temp_dir().join(unique_entity("never-created", "round"));
    let account_id = unique_entity("acc", "missing");
    store
        .upsert_account(&account(&account_id, &tenant, AccountState::Online, None))
        .await
        .expect("account insert");
    store
        .upsert_profile(&NewProfile {
            tenant: &tenant,
            id: &missing_id,
            account_id: &fishpond_domain::AccountId::parse(account_id).expect("account id"),
            label: "ghost",
            storage_path: missing_dir.to_str().expect("utf-8"),
            secret: &SecretBox::new("passphrase"),
            cipher: &cipher_for(&store).await,
            at: ts(3_500),
        })
        .await
        .expect("profile insert");
    let missing = manager
        .backup(&tenant, &missing_id, &alice, &root, term, ts(5_100))
        .await
        .expect_err("missing dir");
    match &missing {
        ProfileError::ProfileDirMissing { path } => {
            assert_eq!(Path::new(path), missing_dir, "{missing:?}")
        }
        other => panic!("expected ProfileDirMissing, got {other:?}"),
    }
}

#[tokio::test]
async fn backup_takes_and_releases_the_exclusive_lock() {
    let Some(store) = gated().await else {
        eprintln!("skip: no database");
        return;
    };
    let tenant = tenant_for(&store, "pm-lock").await;
    let manager = ProfileManager::new(store.clone());
    let (id, _dir) = register_with_dir(&store, &tenant, "lock").await;
    let root = root_for("lock");

    // A live foreign holder blocks the whole operation.
    let bob = holder("bob");
    store
        .acquire_profile_lock(&tenant, &id, &bob, ts(10_000), ts(1_000))
        .await
        .expect("bob holds");
    let alice = holder("alice");
    let blocked = manager
        .backup(&tenant, &id, &alice, &root, 60_000, ts(4_000))
        .await
        .expect_err("live foreign holder");
    assert!(
        blocked.to_string().contains(bob.as_str()),
        "conflict names the holder: {blocked}"
    );
    // Bob steps aside so the next phase starts from a free lock (his
    // grant outlives the operations below).
    store
        .release_profile_lock(&tenant, &id, &bob)
        .await
        .expect("bob releases");

    // Failure paths release the lock: after a failed backup (no
    // snapshot, restored into a fresh root fails with NoBackup) the
    // next holder acquires immediately at generation 1.
    let failed = manager
        .restore(&tenant, &id, &alice, &root, 60_000, ts(4_100))
        .await
        .expect_err("no snapshot to restore");
    assert!(matches!(failed, ProfileError::NoBackup), "{failed:?}");
    let lock = store
        .acquire_profile_lock(&tenant, &id, &bob, ts(20_000), ts(4_200))
        .await
        .expect("lock free after failed operation");
    assert_eq!(lock.generation, 1, "no steal happened — alice released");
    assert!(
        store
            .release_profile_lock(&tenant, &id, &bob)
            .await
            .expect("release")
    );
}

#[tokio::test]
async fn destroy_wipes_the_tree_and_tombstones() {
    let Some(store) = gated().await else {
        eprintln!("skip: no database");
        return;
    };
    let tenant = tenant_for(&store, "pm-destroy").await;
    let manager = ProfileManager::new(store.clone());
    let (id, dir) = register_with_dir(&store, &tenant, "destroy").await;
    let alice = holder("alice");

    manager
        .destroy(&tenant, &id, &alice, 60_000, ts(6_000))
        .await
        .expect("destroy");
    assert!(!dir.exists(), "the on-disk tree is gone: {}", dir.display());
    let row = store
        .get_profile(&tenant, &id)
        .await
        .expect("get")
        .expect("tombstone stays listed");
    assert_eq!(row.state, fishpond_storage::ProfileState::Destroyed);
    assert!(
        store
            .get_profile_lock(&tenant, &id)
            .await
            .expect("lock gone")
            .is_none()
    );

    // A destroyed profile refuses further operations.
    let refused = manager
        .backup(
            &tenant,
            &id,
            &holder("bob"),
            &root_for("destroy"),
            60_000,
            ts(6_100),
        )
        .await
        .expect_err("destroyed profile");
    assert!(refused.to_string().contains("destroyed"), "{refused}");
}

#[tokio::test]
async fn profile_operations_are_tenant_isolated() {
    let Some(store) = gated().await else {
        eprintln!("skip: no database");
        return;
    };
    let tenant_a = tenant_for(&store, "pm-iso-a").await;
    let tenant_b = tenant_for(&store, "pm-iso-b").await;
    let manager = ProfileManager::new(store.clone());
    let (id, _dir) = register_with_dir(&store, &tenant_a, "iso").await;

    // Tenant B cannot lock, backup, restore or destroy A's profile.
    let bob = holder("b");
    let root = root_for("iso-b");
    let backup_err = restore_error(
        manager
            .backup(&tenant_b, &id, &bob, &root, 60_000, ts(7_300))
            .await
            .expect_err("foreign backup"),
    );
    assert!(backup_err.contains("not found"), "{backup_err}");
    assert!(
        manager
            .restore(&tenant_b, &id, &bob, &root, 60_000, ts(7_400))
            .await
            .is_err()
    );
    assert!(
        manager
            .destroy(&tenant_b, &id, &bob, 60_000, ts(7_500))
            .await
            .is_err()
    );

    // A's profile survived B's attempts intact.
    let row = store
        .get_profile(&tenant_a, &id)
        .await
        .expect("get")
        .expect("profile exists");
    assert_eq!(row.state, fishpond_storage::ProfileState::Active);
}
