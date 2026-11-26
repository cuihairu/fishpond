//! Profile manager (F022 / T11, T12): the exclusive-lock discipline
//! and the on-disk lifecycle for browser profiles.
//!
//! Every mutation holds the profile's exclusive lock (the storage CAS
//! with lease-fencing semantics) for the caller-supplied term, and
//! releases it on both outcomes — a failed backup never wedges the
//! profile. The disk side is deliberately boring and verifiable:
//!
//! * **backup** — copies the profile directory into a timestamped
//!   snapshot, hashing every file (SHA-256) into a `manifest.json`
//!   whose `digest` line-hashes the sorted file list; the digest is
//!   recorded in the database row.
//! * **restore** — picks the latest snapshot, re-hashes every file
//!   against the manifest (any drift is [`ProfileError::Tampered`],
//!   naming the file, never its content), wipes the live directory and
//!   copies the snapshot back.
//! * **destroy** — wipes the directory and leaves the row as a
//!   `destroyed` tombstone; the tombstone can never be locked again.
//!
//! The T11/T12 mapping follows the task title wording, as in F012's
//! T-notes: T11 is the exclusive lock, T12 backup/restore/destroy.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use fishpond_domain::{DomainError, TenantId, UnixMillis, WorkerId};
use fishpond_storage::{StorageError, Store};
use sha2::{Digest, Sha256};

/// The manifest written into every snapshot (and its digest rule:
/// SHA-256 over the sorted `path\thex\n` lines).
pub const MANIFEST_NAME: &str = "manifest.json";

/// One snapshot's outcome.
#[derive(Debug, Clone)]
pub struct BackupReport {
    /// Where the snapshot was written.
    pub snapshot_dir: PathBuf,
    /// Files captured (empty directories are not tracked).
    pub file_count: usize,
    /// Total bytes captured.
    pub total_bytes: u64,
    /// The manifest digest recorded in the database row.
    pub digest: String,
}

/// Why a profile operation failed. Errors name paths and states —
/// never file contents or the profile passphrase.
#[derive(Debug)]
pub enum ProfileError {
    /// The store refused an operation (lock CAS, missing profile, ...).
    Storage(StorageError),
    /// A snapshot file (or the manifest itself) did not match its
    /// recorded digest.
    Tampered {
        /// The relative path that drifted (or `manifest.json`).
        path: String,
    },
    /// No snapshot exists to restore.
    NoBackup,
    /// The profile's on-disk directory is missing.
    ProfileDirMissing {
        /// The registered storage path.
        path: String,
    },
    /// A filesystem operation failed.
    Io {
        /// What the code was doing (copying, hashing, ...).
        op: &'static str,
        /// The path involved.
        path: String,
        /// The underlying error.
        source: std::io::Error,
    },
    /// A domain value failed to construct (timestamp bounds).
    Domain(DomainError),
}

impl std::fmt::Display for ProfileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProfileError::Storage(err) => write!(f, "profile store error: {err}"),
            ProfileError::Tampered { path } => {
                write!(f, "backup integrity check failed for {path}")
            }
            ProfileError::NoBackup => write!(f, "no backup snapshot to restore"),
            ProfileError::ProfileDirMissing { path } => {
                write!(f, "profile directory is missing: {path}")
            }
            ProfileError::Io { op, path, .. } => write!(f, "io while {op}: {path}"),
            ProfileError::Domain(err) => write!(f, "profile domain error: {err}"),
        }
    }
}

impl std::error::Error for ProfileError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ProfileError::Storage(err) => Some(err),
            ProfileError::Domain(err) => Some(err),
            ProfileError::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

impl From<StorageError> for ProfileError {
    fn from(err: StorageError) -> Self {
        ProfileError::Storage(err)
    }
}

impl From<DomainError> for ProfileError {
    fn from(err: DomainError) -> Self {
        ProfileError::Domain(err)
    }
}

/// Runs `work` holding the profile lock; the release happens on both
/// outcomes (a release failure is harmless: expiry steals the lock).
macro_rules! hold_lock {
    ($self:expr, $tenant:expr, $profile_id:expr, $holder:expr, $term_ms:expr, $at:expr, $work:expr) => {{
        let expires = UnixMillis::from_millis($at.as_millis() + $term_ms)?;
        $self
            .store
            .acquire_profile_lock($tenant, $profile_id, $holder, expires, $at)
            .await?;
        let result = async { $work }.await;
        // Release on both outcomes. A failed release is harmless: the
        // row survives until expiry and the next holder steals it —
        // it must not shadow the operation's own result.
        let _ = $self
            .store
            .release_profile_lock($tenant, $profile_id, $holder)
            .await;
        result
    }};
}

/// The profile lifecycle service: backup / restore / destroy under the
/// exclusive lock. Stateless — the `Store` owns the pool.
pub struct ProfileManager {
    store: Store,
}

impl ProfileManager {
    /// Wraps a connected store.
    pub fn new(store: Store) -> Self {
        Self { store }
    }

    /// The underlying store (for callers composing other flows).
    pub fn store(&self) -> &Store {
        &self.store
    }

    /// Snapshots the profile directory into
    /// `<backup_root>/<profile_id>/<unix_ms>/`, hashes every file into
    /// `manifest.json`, and records the digest on the profile row.
    pub async fn backup(
        &self,
        tenant: &TenantId,
        profile_id: &str,
        holder: &WorkerId,
        backup_root: &Path,
        term_ms: i64,
        at: UnixMillis,
    ) -> Result<BackupReport, ProfileError> {
        hold_lock!(self, tenant, profile_id, holder, term_ms, at, {
            let profile = self
                .store
                .get_profile(tenant, profile_id)
                .await?
                .ok_or_else(|| StorageError::NotFound {
                    what: "profile",
                    id: profile_id.to_owned(),
                })?;
            let source = PathBuf::from(&profile.storage_path);
            if !source.is_dir() {
                return Err(ProfileError::ProfileDirMissing {
                    path: profile.storage_path.clone(),
                });
            }
            let files = collect_files(&source)
                .map_err(|err| io_err("walking the profile directory", &source, err))?;
            let mut total_bytes = 0u64;
            let mut lines = String::new();
            let mut hashes: BTreeMap<String, String> = BTreeMap::new();
            for (rel, path) in &files {
                let bytes = std::fs::read(path)
                    .map_err(|err| io_err("reading a profile file", path, err))?;
                let hex = sha256_hex(&bytes);
                total_bytes += bytes.len() as u64;
                lines.push_str(&format!("{rel}\t{hex}\n"));
                hashes.insert(rel.clone(), hex);
            }
            let digest = sha256_hex(lines.as_bytes());

            let snapshot_dir = backup_root
                .join(profile_id)
                .join(at.as_millis().to_string());
            std::fs::create_dir_all(&snapshot_dir)
                .map_err(|err| io_err("creating the snapshot directory", &snapshot_dir, err))?;
            for (rel, path) in &files {
                let dest = snapshot_dir.join(rel);
                if let Some(parent) = dest.parent() {
                    std::fs::create_dir_all(parent)
                        .map_err(|err| io_err("creating a snapshot subdirectory", parent, err))?;
                }
                std::fs::copy(path, &dest)
                    .map_err(|err| io_err("copying a profile file", path, err))?;
            }
            let manifest = serde_json::json!({
                "profile": profile_id,
                "created_at": at.as_millis(),
                "digest": digest,
                "files": hashes
                    .iter()
                    .map(|(rel, hex)| serde_json::json!({
                        "path": rel, "sha256": hex,
                    }))
                    .collect::<Vec<_>>(),
            });
            let manifest_path = snapshot_dir.join(MANIFEST_NAME);
            std::fs::write(&manifest_path, manifest.to_string())
                .map_err(|err| io_err("writing the manifest", &manifest_path, err))?;

            self.store
                .record_backup(tenant, profile_id, &digest, at)
                .await?;
            Ok(BackupReport {
                snapshot_dir,
                file_count: files.len(),
                total_bytes,
                digest,
            })
        })
    }

    /// Restores the latest snapshot: every file is re-hashed against
    /// the manifest first (drift is [`ProfileError::Tampered`]), then
    /// the live directory is replaced by the snapshot contents.
    pub async fn restore(
        &self,
        tenant: &TenantId,
        profile_id: &str,
        holder: &WorkerId,
        backup_root: &Path,
        term_ms: i64,
        at: UnixMillis,
    ) -> Result<(), ProfileError> {
        hold_lock!(self, tenant, profile_id, holder, term_ms, at, {
            let profile = self
                .store
                .get_profile(tenant, profile_id)
                .await?
                .ok_or_else(|| StorageError::NotFound {
                    what: "profile",
                    id: profile_id.to_owned(),
                })?;
            let snapshots = backup_root.join(profile_id);
            let latest = if snapshots.is_dir() {
                latest_snapshot(&snapshots)
                    .map_err(|err| io_err("listing snapshots", &snapshots, err))?
                    .ok_or(ProfileError::NoBackup)?
            } else {
                // No snapshot directory means nothing was ever backed
                // up — the same "no backup" contract as an empty one.
                return Err(ProfileError::NoBackup);
            };
            let manifest_path = latest.join(MANIFEST_NAME);
            let manifest = std::fs::read(&manifest_path)
                .map_err(|err| io_err("reading the manifest", &manifest_path, err))?;
            let manifest: serde_json::Value =
                serde_json::from_slice(&manifest).map_err(|_| ProfileError::Tampered {
                    path: MANIFEST_NAME.to_owned(),
                })?;
            if manifest["profile"].as_str() != Some(profile_id) {
                return Err(ProfileError::Tampered {
                    path: MANIFEST_NAME.to_owned(),
                });
            }
            let mut lines = String::new();
            let listed = manifest["files"]
                .as_array()
                .ok_or_else(|| ProfileError::Tampered {
                    path: MANIFEST_NAME.to_owned(),
                })?;
            for entry in listed {
                let rel = entry["path"]
                    .as_str()
                    .ok_or_else(|| ProfileError::Tampered {
                        path: MANIFEST_NAME.to_owned(),
                    })?
                    .to_owned();
                let recorded = entry["sha256"]
                    .as_str()
                    .ok_or_else(|| ProfileError::Tampered {
                        path: MANIFEST_NAME.to_owned(),
                    })?
                    .to_owned();
                let path = latest.join(&rel);
                let bytes = std::fs::read(&path)
                    .map_err(|err| io_err("reading a snapshot file", &path, err))?;
                let actual = sha256_hex(&bytes);
                if actual != recorded {
                    return Err(ProfileError::Tampered { path: rel });
                }
                lines.push_str(&format!("{rel}\t{actual}\n"));
            }
            if sha256_hex(lines.as_bytes()) != manifest["digest"].as_str().unwrap_or_default() {
                return Err(ProfileError::Tampered {
                    path: MANIFEST_NAME.to_owned(),
                });
            }

            // Verified: replace the live directory with the snapshot.
            let live = PathBuf::from(&profile.storage_path);
            if live.exists() {
                std::fs::remove_dir_all(&live)
                    .map_err(|err| io_err("clearing the profile directory", &live, err))?;
            }
            std::fs::create_dir_all(&live)
                .map_err(|err| io_err("recreating the profile directory", &live, err))?;
            for entry in listed {
                let rel = entry["path"]
                    .as_str()
                    .ok_or_else(|| ProfileError::Tampered {
                        path: MANIFEST_NAME.to_owned(),
                    })?;
                let from = latest.join(rel);
                let to = live.join(rel);
                if let Some(parent) = to.parent() {
                    std::fs::create_dir_all(parent)
                        .map_err(|err| io_err("recreating a subdirectory", parent, err))?;
                }
                std::fs::copy(&from, &to).map_err(|err| io_err("restoring a file", &to, err))?;
            }
            self.store
                .mark_profile_restored(tenant, profile_id, at)
                .await?;
            Ok(())
        })
    }

    /// Destroys the profile: the directory tree is wiped and the row
    /// becomes a `destroyed` tombstone (lock row removed by the
    /// store). A destroyed profile refuses further operations.
    pub async fn destroy(
        &self,
        tenant: &TenantId,
        profile_id: &str,
        holder: &WorkerId,
        term_ms: i64,
        at: UnixMillis,
    ) -> Result<(), ProfileError> {
        hold_lock!(self, tenant, profile_id, holder, term_ms, at, {
            let profile = self
                .store
                .get_profile(tenant, profile_id)
                .await?
                .ok_or_else(|| StorageError::NotFound {
                    what: "profile",
                    id: profile_id.to_owned(),
                })?;
            let live = PathBuf::from(&profile.storage_path);
            if live.exists() {
                std::fs::remove_dir_all(&live)
                    .map_err(|err| io_err("wiping the profile directory", &live, err))?;
            }
            self.store.destroy_profile(tenant, profile_id, at).await?;
            Ok(())
        })
    }
}

/// The highest-numbered snapshot directory, if any.
fn latest_snapshot(snapshots: &Path) -> std::io::Result<Option<PathBuf>> {
    let mut best: Option<(u64, PathBuf)> = None;
    for entry in std::fs::read_dir(snapshots)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        let Ok(stamp) = name.parse::<u64>() else {
            continue;
        };
        let path = entry.path();
        if path.is_dir() && best.as_ref().is_none_or(|(seen, _)| stamp > *seen) {
            best = Some((stamp, path));
        }
    }
    Ok(best.map(|(_, path)| path))
}

/// Relative paths of every regular file under `root`, sorted (the
/// `BTreeMap` gives both the sort and the dedup).
fn collect_files(root: &Path) -> std::io::Result<BTreeMap<String, PathBuf>> {
    let mut files = BTreeMap::new();
    collect_into(root, root, &mut files)?;
    Ok(files)
}

fn collect_into(
    root: &Path,
    dir: &Path,
    files: &mut BTreeMap<String, PathBuf>,
) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            collect_into(root, &path, files)?;
        } else if path.is_file() {
            let rel = path
                .strip_prefix(root)
                .expect("walked paths live under the root")
                .to_string_lossy()
                .replace('\\', "/");
            files.insert(rel, path);
        }
    }
    Ok(())
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let digest = hasher.finalize();
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

fn io_err(op: &'static str, path: &Path, err: std::io::Error) -> ProfileError {
    ProfileError::Io {
        op,
        path: path.display().to_string(),
        source: err,
    }
}
