//! Browser profile registry (F022): metadata, lifecycle state and the
//! exclusive-use lock.
//!
//! The database holds profile *metadata* only — label, on-disk path,
//! state, an optional F008-sealed passphrase and backup bookkeeping.
//! The profile's own contents live on disk and never enter a row. The
//! lock mirrors the account-lease CAS discipline: at most one live
//! holder, `generation` as the fencing token, expiry as the steal
//! path. A `destroyed` profile is a tombstone: its row stays for
//! audit and its lock can never be taken again.

use crate::credentials::{CredentialCipher, SealedSecret, SecretBox};
use crate::error::{StorageError, StorageResult};
use crate::store::Store;
use fishpond_domain::{AccountId, TenantId, UnixMillis, WorkerId};
use sqlx::Row;

/// A profile's lifecycle state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProfileState {
    /// In use (or ready for use) on disk.
    Active,
    /// A verified backup exists (digest recorded).
    BackedUp,
    /// Destroyed: contents wiped, row kept as a tombstone.
    Destroyed,
}

impl ProfileState {
    /// The canonical wire/storage spelling.
    pub fn as_str(&self) -> &'static str {
        match self {
            ProfileState::Active => "active",
            ProfileState::BackedUp => "backed_up",
            ProfileState::Destroyed => "destroyed",
        }
    }

    /// Parses the canonical spelling.
    pub fn parse(raw: &str) -> StorageResult<Self> {
        match raw {
            "active" => Ok(ProfileState::Active),
            "backed_up" => Ok(ProfileState::BackedUp),
            "destroyed" => Ok(ProfileState::Destroyed),
            other => Err(StorageError::Invalid {
                what: "profile state",
                reason: format!("unknown state {other}"),
            }),
        }
    }
}

/// A registered profile's metadata (no secrets).
#[derive(Debug, Clone)]
pub struct ProfileRecord {
    /// Owning tenant.
    pub tenant_id: TenantId,
    /// Profile id (unique per tenant).
    pub id: String,
    /// The automation account this profile belongs to.
    pub account_id: AccountId,
    /// Human label.
    pub label: String,
    /// Lifecycle state.
    pub state: ProfileState,
    /// On-disk directory holding the profile contents.
    pub storage_path: String,
    /// Fingerprint of the sealed passphrase (never the secret).
    pub secret_fingerprint: String,
    /// Digest of the last recorded backup (None until backed up).
    pub backup_digest: Option<String>,
    /// When the last backup was recorded.
    pub last_backup_at: Option<UnixMillis>,
    /// Creation instant.
    pub created_at: UnixMillis,
    /// Last mutation instant.
    pub updated_at: UnixMillis,
}

/// Registration input: the passphrase is sealed before it touches a row.
pub struct NewProfile<'a> {
    /// Owning tenant.
    pub tenant: &'a TenantId,
    /// Profile id (unique per tenant).
    pub id: &'a str,
    /// The automation account this profile belongs to.
    pub account_id: &'a AccountId,
    /// Human label.
    pub label: &'a str,
    /// On-disk directory holding the profile contents.
    pub storage_path: &'a str,
    /// The profile passphrase (sealed by `cipher` before insert).
    pub secret: &'a SecretBox,
    /// The tenant credential cipher (F008 envelope).
    pub cipher: &'a CredentialCipher,
    /// Registration instant.
    pub at: UnixMillis,
}

/// The exclusive-use lock on a profile (fencing semantics as F012).
#[derive(Debug, Clone)]
pub struct ProfileLock {
    /// Owning tenant.
    pub tenant_id: TenantId,
    /// Locked profile id.
    pub profile_id: String,
    /// Current holder (worker id).
    pub holder: String,
    /// Fencing token; +1 on every holder change.
    pub generation: u64,
    /// When the term ends (the steal boundary).
    pub expires_at: UnixMillis,
    /// When this term was acquired.
    pub acquired_at: UnixMillis,
}

/// The F008 column convention: the `secret_*` columns carry the data
/// envelope's nonce/cipher; `dek_*`/`wrapped_dek` the DEK wrap.
struct SecretColumns {
    secret_nonce: Vec<u8>,
    secret_cipher: Vec<u8>,
    dek_nonce: Vec<u8>,
    wrapped_dek: Vec<u8>,
    kek_version: i32,
    fingerprint: String,
}

fn seal_secret(cipher: &CredentialCipher, secret: &SecretBox) -> StorageResult<SecretColumns> {
    let sealed = cipher.seal(secret.expose())?;
    Ok(SecretColumns {
        secret_nonce: sealed.data_nonce,
        secret_cipher: sealed.data_cipher,
        dek_nonce: sealed.dek_nonce,
        wrapped_dek: sealed.wrapped_dek,
        kek_version: sealed.kek_version,
        fingerprint: secret.fingerprint(),
    })
}

const PROFILE_COLS: &str = "tenant_id, id, account_id, label, state, storage_path, \
    secret_fingerprint, backup_digest, last_backup_at, created_at, updated_at";

fn map_profile(row: &sqlx::postgres::PgRow) -> StorageResult<ProfileRecord> {
    Ok(ProfileRecord {
        tenant_id: TenantId::parse(row.try_get::<String, _>("tenant_id")?)?,
        id: row.try_get::<String, _>("id")?,
        account_id: AccountId::parse(row.try_get::<String, _>("account_id")?)?,
        label: row.try_get::<String, _>("label")?,
        state: ProfileState::parse(&row.try_get::<String, _>("state")?)?,
        storage_path: row.try_get::<String, _>("storage_path")?,
        secret_fingerprint: row.try_get::<String, _>("secret_fingerprint")?,
        backup_digest: row.try_get::<Option<String>, _>("backup_digest")?,
        last_backup_at: row
            .try_get::<Option<i64>, _>("last_backup_at")?
            .map(UnixMillis::from_millis)
            .transpose()?,
        created_at: UnixMillis::from_millis(row.try_get::<i64, _>("created_at")?)?,
        updated_at: UnixMillis::from_millis(row.try_get::<i64, _>("updated_at")?)?,
    })
}

fn map_lock(row: &sqlx::postgres::PgRow) -> StorageResult<ProfileLock> {
    Ok(ProfileLock {
        tenant_id: TenantId::parse(row.try_get::<String, _>("tenant_id")?)?,
        profile_id: row.try_get::<String, _>("profile_id")?,
        holder: row.try_get::<String, _>("holder")?,
        generation: row.try_get::<i64, _>("generation")? as u64,
        expires_at: UnixMillis::from_millis(row.try_get::<i64, _>("expires_at")?)?,
        acquired_at: UnixMillis::from_millis(row.try_get::<i64, _>("acquired_at")?)?,
    })
}

impl Store {
    /// Registers a profile, sealing its passphrase. Re-registering the
    /// same id rotates label/path/secret but keeps lifecycle state and
    /// backup bookkeeping.
    pub async fn upsert_profile(&self, profile: &NewProfile<'_>) -> StorageResult<ProfileRecord> {
        let columns = seal_secret(profile.cipher, profile.secret)?;
        let mut tx = self.begin_tenant(profile.tenant).await?;
        let row = sqlx::query(
            "INSERT INTO profiles
                (tenant_id, id, account_id, label, state, storage_path,
                 secret_nonce, secret_cipher, dek_nonce, wrapped_dek, kek_version,
                 secret_fingerprint, created_at, updated_at)
             VALUES ($1, $2, $3, $4, 'active', $5, $6, $7, $8, $9, $10, $11, $12, $12)
             ON CONFLICT (tenant_id, id) DO UPDATE SET
                account_id = EXCLUDED.account_id,
                label = EXCLUDED.label,
                storage_path = EXCLUDED.storage_path,
                secret_nonce = EXCLUDED.secret_nonce,
                secret_cipher = EXCLUDED.secret_cipher,
                dek_nonce = EXCLUDED.dek_nonce,
                wrapped_dek = EXCLUDED.wrapped_dek,
                kek_version = EXCLUDED.kek_version,
                secret_fingerprint = EXCLUDED.secret_fingerprint,
                updated_at = EXCLUDED.updated_at
             RETURNING {PROFILE_COLS}",
        )
        .bind(profile.tenant.as_str())
        .bind(profile.id)
        .bind(profile.account_id.as_str())
        .bind(profile.label)
        .bind(profile.storage_path)
        .bind(&columns.secret_nonce)
        .bind(&columns.secret_cipher)
        .bind(&columns.dek_nonce)
        .bind(&columns.wrapped_dek)
        .bind(columns.kek_version)
        .bind(&columns.fingerprint)
        .bind(profile.at.as_millis())
        .fetch_one(&mut *tx)
        .await?;
        tx.commit().await?;
        map_profile(&row)
    }

    /// The profile's metadata, or `None` when absent (or foreign).
    pub async fn get_profile(
        &self,
        tenant: &TenantId,
        id: &str,
    ) -> StorageResult<Option<ProfileRecord>> {
        let mut tx = self.begin_tenant(tenant).await?;
        let row = sqlx::query(&format!(
            "SELECT {PROFILE_COLS} FROM profiles WHERE tenant_id = $1 AND id = $2"
        ))
        .bind(tenant.as_str())
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?;
        tx.commit().await?;
        row.map(|row| map_profile(&row)).transpose()
    }

    /// Every profile in the tenant (destroyed tombstones included).
    pub async fn list_profiles(&self, tenant: &TenantId) -> StorageResult<Vec<ProfileRecord>> {
        let mut tx = self.begin_tenant(tenant).await?;
        let rows = sqlx::query(&format!(
            "SELECT {PROFILE_COLS} FROM profiles WHERE tenant_id = $1 ORDER BY id"
        ))
        .bind(tenant.as_str())
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;
        rows.iter().map(map_profile).collect()
    }

    /// Opens the profile's sealed passphrase (the F008 envelope from
    /// the `secret_*`/`dek_*` columns).
    pub async fn open_profile_secret(
        &self,
        tenant: &TenantId,
        id: &str,
        cipher: &CredentialCipher,
    ) -> StorageResult<SecretBox> {
        let mut tx = self.begin_tenant(tenant).await?;
        let row = sqlx::query(
            "SELECT secret_nonce, secret_cipher, dek_nonce, wrapped_dek, kek_version
             FROM profiles WHERE tenant_id = $1 AND id = $2",
        )
        .bind(tenant.as_str())
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?;
        tx.commit().await?;
        let row = row.ok_or_else(|| StorageError::NotFound {
            what: "profile",
            id: id.to_owned(),
        })?;
        let sealed = SealedSecret {
            kek_version: row.try_get::<i32, _>("kek_version")?,
            dek_nonce: row.try_get::<Vec<u8>, _>("dek_nonce")?,
            wrapped_dek: row.try_get::<Vec<u8>, _>("wrapped_dek")?,
            data_nonce: row.try_get::<Vec<u8>, _>("secret_nonce")?,
            data_cipher: row.try_get::<Vec<u8>, _>("secret_cipher")?,
        };
        cipher.open(&sealed)
    }

    /// Records a verified backup: digest bookkeeping and the
    /// `backed_up` state. A destroyed profile is never updated.
    pub async fn record_backup(
        &self,
        tenant: &TenantId,
        id: &str,
        digest: &str,
        at: UnixMillis,
    ) -> StorageResult<ProfileRecord> {
        let mut tx = self.begin_tenant(tenant).await?;
        let row = sqlx::query(&format!(
            "UPDATE profiles SET state = 'backed_up', backup_digest = $3,
                    last_backup_at = $4, updated_at = $4
             WHERE tenant_id = $1 AND id = $2 AND state <> 'destroyed'
             RETURNING {PROFILE_COLS}"
        ))
        .bind(tenant.as_str())
        .bind(id)
        .bind(digest)
        .bind(at.as_millis())
        .fetch_optional(&mut *tx)
        .await?;
        tx.commit().await?;
        let row = row.ok_or_else(|| StorageError::NotFound {
            what: "profile",
            id: id.to_owned(),
        })?;
        map_profile(&row)
    }

    /// Marks a restored profile back in use (`active`); the backup
    /// bookkeeping stays.
    pub async fn mark_profile_restored(
        &self,
        tenant: &TenantId,
        id: &str,
        at: UnixMillis,
    ) -> StorageResult<ProfileRecord> {
        let mut tx = self.begin_tenant(tenant).await?;
        let row = sqlx::query(&format!(
            "UPDATE profiles SET state = 'active', updated_at = $3
             WHERE tenant_id = $1 AND id = $2 AND state <> 'destroyed'
             RETURNING {PROFILE_COLS}"
        ))
        .bind(tenant.as_str())
        .bind(id)
        .bind(at.as_millis())
        .fetch_optional(&mut *tx)
        .await?;
        tx.commit().await?;
        let row = row.ok_or_else(|| StorageError::NotFound {
            what: "profile",
            id: id.to_owned(),
        })?;
        map_profile(&row)
    }

    /// Destroys a profile: a `destroyed` tombstone (row kept for
    /// audit) and the lock row removed. The on-disk wipe is the
    /// caller's job — this only closes the metadata.
    pub async fn destroy_profile(
        &self,
        tenant: &TenantId,
        id: &str,
        at: UnixMillis,
    ) -> StorageResult<ProfileRecord> {
        let mut tx = self.begin_tenant(tenant).await?;
        sqlx::query("DELETE FROM profile_locks WHERE tenant_id = $1 AND profile_id = $2")
            .bind(tenant.as_str())
            .bind(id)
            .execute(&mut *tx)
            .await?;
        let row = sqlx::query(&format!(
            "UPDATE profiles SET state = 'destroyed', updated_at = $3
             WHERE tenant_id = $1 AND id = $2
             RETURNING {PROFILE_COLS}"
        ))
        .bind(tenant.as_str())
        .bind(id)
        .bind(at.as_millis())
        .fetch_optional(&mut *tx)
        .await?;
        tx.commit().await?;
        let row = row.ok_or_else(|| StorageError::NotFound {
            what: "profile",
            id: id.to_owned(),
        })?;
        map_profile(&row)
    }

    /// Acquires the profile's exclusive lock until `expires_at`:
    /// succeeds when unlocked, expired (steal, generation +1) or held
    /// by `holder` (re-extend, generation unchanged). A live foreign
    /// lock is a [`StorageError::Conflict`] naming holder and
    /// generation. A destroyed (or absent) profile is never lockable.
    pub async fn acquire_profile_lock(
        &self,
        tenant: &TenantId,
        profile_id: &str,
        holder: &WorkerId,
        expires_at: UnixMillis,
        at: UnixMillis,
    ) -> StorageResult<ProfileLock> {
        let mut tx = self.begin_tenant(tenant).await?;
        let row = sqlx::query(
            "INSERT INTO profile_locks
                 (tenant_id, profile_id, holder, generation, expires_at, acquired_at)
             SELECT $1, $2, $3, 1, $4, $5
             WHERE EXISTS (
                 SELECT 1 FROM profiles
                 WHERE tenant_id = $1 AND id = $2 AND state <> 'destroyed')
             ON CONFLICT (tenant_id, profile_id) DO UPDATE SET
                 holder = $3,
                 generation = CASE WHEN profile_locks.holder = $3
                                   THEN profile_locks.generation
                                   ELSE profile_locks.generation + 1 END,
                 expires_at = $4,
                 acquired_at = $5
             WHERE profile_locks.holder = $3 OR profile_locks.expires_at <= $5
             RETURNING tenant_id, profile_id, holder, generation, expires_at, acquired_at",
        )
        .bind(tenant.as_str())
        .bind(profile_id)
        .bind(holder.as_str())
        .bind(expires_at.as_millis())
        .bind(at.as_millis())
        .fetch_optional(&mut *tx)
        .await?;
        tx.commit().await?;
        match row {
            Some(row) => map_lock(&row),
            None => {
                // The CAS lost: either a live foreign holder or the
                // profile is destroyed/absent. Name which.
                let profile = self.get_profile(tenant, profile_id).await?.ok_or_else(|| {
                    StorageError::NotFound {
                        what: "profile",
                        id: profile_id.to_owned(),
                    }
                })?;
                if profile.state == ProfileState::Destroyed {
                    return Err(StorageError::Conflict {
                        what: "profile lock",
                        id: profile_id.to_owned(),
                        reason: "profile is destroyed".to_owned(),
                    });
                }
                let current = self
                    .get_profile_lock(tenant, profile_id)
                    .await?
                    .ok_or_else(|| StorageError::Conflict {
                        what: "profile lock",
                        id: profile_id.to_owned(),
                        reason: "lock vanished while acquiring".to_owned(),
                    })?;
                Err(StorageError::Conflict {
                    what: "profile lock",
                    id: profile_id.to_owned(),
                    reason: format!(
                        "held by {} (generation {})",
                        current.holder, current.generation
                    ),
                })
            }
        }
    }

    /// Extends the holder's own lock; generation unchanged. An expired
    /// term is never extended in place — re-acquire (fencing).
    pub async fn renew_profile_lock(
        &self,
        tenant: &TenantId,
        profile_id: &str,
        holder: &WorkerId,
        expires_at: UnixMillis,
        at: UnixMillis,
    ) -> StorageResult<ProfileLock> {
        let mut tx = self.begin_tenant(tenant).await?;
        let row = sqlx::query(
            "UPDATE profile_locks SET expires_at = $4
             WHERE tenant_id = $1 AND profile_id = $2
                   AND holder = $3 AND expires_at > $5
             RETURNING tenant_id, profile_id, holder, generation, expires_at, acquired_at",
        )
        .bind(tenant.as_str())
        .bind(profile_id)
        .bind(holder.as_str())
        .bind(expires_at.as_millis())
        .bind(at.as_millis())
        .fetch_optional(&mut *tx)
        .await?;
        tx.commit().await?;
        match row {
            Some(row) => map_lock(&row),
            None => match self.get_profile_lock(tenant, profile_id).await? {
                None => Err(StorageError::NotFound {
                    what: "profile lock",
                    id: profile_id.to_owned(),
                }),
                Some(current) if current.holder != holder.as_str() => Err(StorageError::Conflict {
                    what: "profile lock",
                    id: profile_id.to_owned(),
                    reason: format!(
                        "held by {} (generation {})",
                        current.holder, current.generation
                    ),
                }),
                Some(_) => Err(StorageError::Conflict {
                    what: "profile lock",
                    id: profile_id.to_owned(),
                    reason: "term expired; re-acquire".to_owned(),
                }),
            },
        }
    }

    /// Releases the holder's lock (row removed). `Ok(false)` when the
    /// holder did not hold it.
    pub async fn release_profile_lock(
        &self,
        tenant: &TenantId,
        profile_id: &str,
        holder: &WorkerId,
    ) -> StorageResult<bool> {
        let mut tx = self.begin_tenant(tenant).await?;
        let result = sqlx::query(
            "DELETE FROM profile_locks
             WHERE tenant_id = $1 AND profile_id = $2 AND holder = $3",
        )
        .bind(tenant.as_str())
        .bind(profile_id)
        .bind(holder.as_str())
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(result.rows_affected() > 0)
    }

    /// The profile's current lock, or `None`.
    pub async fn get_profile_lock(
        &self,
        tenant: &TenantId,
        profile_id: &str,
    ) -> StorageResult<Option<ProfileLock>> {
        let mut tx = self.begin_tenant(tenant).await?;
        let row = sqlx::query(
            "SELECT tenant_id, profile_id, holder, generation, expires_at, acquired_at
             FROM profile_locks WHERE tenant_id = $1 AND profile_id = $2",
        )
        .bind(tenant.as_str())
        .bind(profile_id)
        .fetch_optional(&mut *tx)
        .await?;
        tx.commit().await?;
        row.map(|row| map_lock(&row)).transpose()
    }
}
