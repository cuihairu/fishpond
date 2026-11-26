//! Credential store: envelope encryption, key rotation and revocation
//! (F008).
//!
//! Envelope layout: every credential gets its own random DEK (data
//! encryption key); the DEK encrypts the secret with AES-256-GCM and is
//! itself wrapped (encrypted) under the master key (KEK). Rotating the
//! master key therefore only re-wraps DEKs — the data ciphertext never
//! moves — and rotating a credential's secret re-seals it under a fresh
//! DEK. A revoked credential refuses to open.
//!
//! Secrets and keys are zeroized on drop; [`SecretBox`] and
//! [`SealedSecret`] `Debug`-print fingerprints and lengths only, so
//! `tracing::debug!` of records can never leak plaintext (tested). Errors
//! name the failure, never the failing material.

use aes_gcm::aead::consts::U12;
use aes_gcm::aead::{Aead, AeadCore, KeyInit, OsRng};
use aes_gcm::{Aes256Gcm, Nonce};
use fishpond_domain::{TenantId, UnixMillis};
use sha2::{Digest, Sha256};
use sqlx::Row;
use zeroize::Zeroizing;

use crate::error::{StorageError, StorageResult};

/// Length of the AES-256 key material in bytes.
const KEY_LEN: usize = 32;
/// Length of a GCM nonce in bytes.
const NONCE_LEN: usize = 12;

/// The master key (KEK) that wraps per-credential DEKs.
///
/// The material is zeroized when the value is dropped.
#[derive(Clone)]
pub struct MasterKey(Zeroizing<[u8; KEY_LEN]>);

impl std::fmt::Debug for MasterKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Fingerprint only: the material itself must never render.
        write!(f, "MasterKey(#{})", fingerprint(&self.0[..]))
    }
}

impl MasterKey {
    /// Generates a fresh random key.
    pub fn generate() -> Self {
        let key = Aes256Gcm::generate_key(OsRng);
        let mut material = [0u8; KEY_LEN];
        material.copy_from_slice(&key);
        Self(Zeroizing::new(material))
    }

    /// Parses a 64-char hex-encoded key (as supplied by configuration).
    pub fn from_hex(raw: &str) -> StorageResult<Self> {
        let bytes = hex_decode(raw).ok_or_else(|| StorageError::Invalid {
            what: "master key",
            reason: "must be 64 hex characters (32 bytes)".to_owned(),
        })?;
        let material: [u8; KEY_LEN] =
            bytes
                .try_into()
                .map_err(|bytes: Vec<u8>| StorageError::Invalid {
                    what: "master key",
                    reason: format!("must be {KEY_LEN} bytes, got {}", bytes.len()),
                })?;
        Ok(Self(Zeroizing::new(material)))
    }

    /// Reads a hex key from an environment variable (the deployment
    /// channel for master keys; the name is expected to point at a secret
    /// mount, never a literal in code).
    pub fn from_env(var: &str) -> StorageResult<Self> {
        let raw = std::env::var(var).map_err(|_| StorageError::Invalid {
            what: "master key",
            reason: format!("environment variable {var} is not set"),
        })?;
        Self::from_hex(raw.trim())
    }

    /// Short fingerprint for logs and screens.
    pub fn fingerprint(&self) -> String {
        fingerprint(&self.0[..])
    }
}

/// The per-credential cryptographic envelope: a wrapped DEK plus the
/// DEK-encrypted secret.
#[derive(Clone, PartialEq, Eq)]
pub struct SealedSecret {
    /// Which master-key generation wrapped the DEK.
    pub kek_version: i32,
    /// Nonce of the DEK wrap.
    pub dek_nonce: Vec<u8>,
    /// The DEK, encrypted under the master key.
    pub wrapped_dek: Vec<u8>,
    /// Nonce of the data encryption.
    pub data_nonce: Vec<u8>,
    /// The secret, encrypted under the DEK.
    pub data_cipher: Vec<u8>,
}

impl std::fmt::Debug for SealedSecret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Lengths only: ciphertext is not plaintext, but there is no
        // reason for anything byte-valued to reach a log line.
        write!(
            f,
            "SealedSecret {{ kek_version: {}, wrapped_dek: {}B, data_cipher: {}B }}",
            self.kek_version,
            self.wrapped_dek.len(),
            self.data_cipher.len()
        )
    }
}

/// A decrypted secret held in zeroizing memory.
pub struct SecretBox(Zeroizing<String>);

impl std::fmt::Debug for SecretBox {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "SecretBox(#{})", fingerprint(self.0.as_bytes()))
    }
}

impl SecretBox {
    /// Wraps raw secret material.
    pub fn new(raw: impl Into<String>) -> Self {
        Self(Zeroizing::new(raw.into()))
    }

    /// The plaintext secret. Use sparingly — hand it to the provider
    /// call site, never to a formatter.
    pub fn expose(&self) -> &str {
        &self.0
    }

    /// Short fingerprint for logs and correlation.
    pub fn fingerprint(&self) -> String {
        fingerprint(self.0.as_bytes())
    }
}

/// Encrypts and unwraps credentials under one master-key generation.
pub struct CredentialCipher {
    kek: MasterKey,
    kek_version: i32,
}

impl std::fmt::Debug for CredentialCipher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "CredentialCipher {{ kek: {:?}, kek_version: {} }}",
            self.kek, self.kek_version
        )
    }
}

impl CredentialCipher {
    /// Builds a cipher over `kek`, tagged with its generation `version`.
    pub fn new(kek: MasterKey, version: i32) -> Self {
        Self {
            kek,
            kek_version: version,
        }
    }

    /// The key generation this cipher represents.
    pub fn kek_version(&self) -> i32 {
        self.kek_version
    }

    /// Seals `secret` under a freshly generated DEK.
    pub fn seal(&self, secret: &str) -> StorageResult<SealedSecret> {
        let dek = MasterKey::generate();
        let data_nonce = random_nonce();
        let data_cipher = encrypt(&dek, &data_nonce, secret.as_bytes())?;
        let dek_nonce = random_nonce();
        let wrapped_dek = encrypt(&self.kek, &dek_nonce, &dek.0[..])?;

        Ok(SealedSecret {
            kek_version: self.kek_version,
            dek_nonce,
            wrapped_dek,
            data_nonce,
            data_cipher,
        })
    }

    /// Unwraps the DEK with this cipher's key and decrypts the secret.
    /// Revocation is checked by the persistence layer, not here.
    pub fn open(&self, sealed: &SealedSecret) -> StorageResult<SecretBox> {
        let dek_bytes = decrypt(&self.kek, &sealed.dek_nonce, &sealed.wrapped_dek)?;
        let dek = MasterKey(Zeroizing::new(dek_bytes.as_slice().try_into().map_err(
            |_| StorageError::Invalid {
                what: "sealed secret",
                reason: format!("wrapped DEK is not {KEY_LEN} bytes"),
            },
        )?));
        let plain = decrypt(&dek, &sealed.data_nonce, &sealed.data_cipher)?;
        let plain = String::from_utf8(plain).map_err(|_| StorageError::Invalid {
            what: "sealed secret",
            reason: "decrypted secret is not valid utf-8".to_owned(),
        })?;
        Ok(SecretBox(Zeroizing::new(plain)))
    }

    /// Re-wraps the DEK under `target`'s master key. The data ciphertext,
    /// nonce and DEK are untouched — rotation must not move data.
    pub fn rewrap(
        &self,
        sealed: &SealedSecret,
        target: &CredentialCipher,
    ) -> StorageResult<SealedSecret> {
        let dek_bytes = decrypt(&self.kek, &sealed.dek_nonce, &sealed.wrapped_dek)?;
        let dek = MasterKey(Zeroizing::new(dek_bytes.as_slice().try_into().map_err(
            |_| StorageError::Invalid {
                what: "sealed secret",
                reason: format!("wrapped DEK is not {KEY_LEN} bytes"),
            },
        )?));
        let dek_nonce = random_nonce();
        let wrapped_dek = encrypt(&target.kek, &dek_nonce, &dek.0[..])?;
        Ok(SealedSecret {
            kek_version: target.kek_version,
            dek_nonce,
            wrapped_dek,
            data_nonce: sealed.data_nonce.clone(),
            data_cipher: sealed.data_cipher.clone(),
        })
    }
}

/// Encrypts `plain` under `key` with `nonce` (AES-256-GCM).
fn encrypt(key: &MasterKey, nonce: &[u8], plain: &[u8]) -> StorageResult<Vec<u8>> {
    let cipher = Aes256Gcm::new((&key.0[..]).into());
    cipher
        .encrypt(&nonce_of(nonce)?, plain)
        .map_err(|_| StorageError::Invalid {
            what: "secret",
            reason: "encryption failed".to_owned(),
        })
}

/// Decrypts `cipher` under `key` with `nonce`.
fn decrypt(key: &MasterKey, nonce: &[u8], cipher: &[u8]) -> StorageResult<Vec<u8>> {
    let aes = Aes256Gcm::new((&key.0[..]).into());
    aes.decrypt(&nonce_of(nonce)?, cipher)
        .map_err(|_| StorageError::Invalid {
            what: "sealed secret",
            reason: "decryption failed (wrong key or tampered data)".to_owned(),
        })
}

/// Validates the nonce length and clones it into a GCM nonce value.
fn nonce_of(bytes: &[u8]) -> StorageResult<Nonce<U12>> {
    if bytes.len() != NONCE_LEN {
        return Err(StorageError::Invalid {
            what: "sealed secret",
            reason: format!("nonce must be {NONCE_LEN} bytes"),
        });
    }
    Ok(Nonce::clone_from_slice(bytes))
}

/// A fresh random GCM nonce.
fn random_nonce() -> Vec<u8> {
    Aes256Gcm::generate_nonce(OsRng).to_vec()
}

/// Eight hex chars of the SHA-256 of `bytes`: enough to correlate, not
/// enough to recover.
fn fingerprint(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    digest[..4].iter().map(|b| format!("{b:02x}")).collect()
}

/// Hex decode with strict length and charset.
fn hex_decode(raw: &str) -> Option<Vec<u8>> {
    let raw = raw.trim();
    if raw.is_empty() || !raw.len().is_multiple_of(2) || !raw.bytes().all(|b| b.is_ascii_hexdigit())
    {
        return None;
    }
    (0..raw.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&raw[i..i + 2], 16).ok())
        .collect()
}

/// A stored credential's metadata: everything but the secret material.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CredentialRecord {
    /// Credential id.
    pub id: String,
    /// Owning tenant.
    pub tenant_id: TenantId,
    /// Platform the credential authenticates to.
    pub platform: String,
    /// Credential kind (e.g. "api_token").
    pub kind: String,
    /// Master-key generation the DEK is wrapped under.
    pub kek_version: i32,
    /// Secret fingerprint (for correlation; never the secret).
    pub fingerprint: Option<String>,
    /// Creation instant.
    pub created_at: UnixMillis,
    /// Whether it was revoked.
    pub is_revoked: bool,
    /// Revocation instant, if revoked.
    pub revoked_at: Option<UnixMillis>,
}

/// What one credential revocation did (see
/// [`Store::revoke_credential_with_sessions`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CredentialRevocation {
    /// The revoked credential's id.
    pub credential: String,
    /// `true` when this call performed the revocation; `false` when the
    /// credential was already revoked (idempotent replay).
    pub revoked: bool,
    /// The revocation instant (the original one on a replay).
    pub revoked_at: UnixMillis,
    /// `AUTHORIZED` sessions carrying the credential that were revoked
    /// with it (zero on a replay).
    pub revoked_sessions: u64,
}

/// Everything needed to issue one credential.
pub struct NewCredential<'a> {
    /// Credential id (unique across tenants).
    pub id: &'a str,
    /// Owning tenant.
    pub tenant: &'a TenantId,
    /// Platform the credential authenticates to.
    pub platform: &'a str,
    /// Credential kind (e.g. "api_token", "cookie").
    pub kind: &'a str,
    /// The cipher whose master key seals the secret.
    pub cipher: &'a CredentialCipher,
    /// The secret material (sealed before it touches the database).
    pub secret: SecretBox,
    /// Creation instant.
    pub created_at: UnixMillis,
}

impl crate::store::Store {
    /// Registers a master-key generation (idempotent per version).
    pub async fn register_master_key(
        &self,
        version: i32,
        created_at: UnixMillis,
    ) -> StorageResult<()> {
        sqlx::query(
            "INSERT INTO master_keys (version, created_at) VALUES ($1, $2)
             ON CONFLICT (version) DO NOTHING",
        )
        .bind(version)
        .bind(created_at.as_millis())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Lists registered master-key generations, oldest first.
    pub async fn list_master_keys(&self) -> StorageResult<Vec<(i32, UnixMillis)>> {
        let rows = sqlx::query("SELECT version, created_at FROM master_keys ORDER BY version")
            .fetch_all(&self.pool)
            .await?;
        rows.iter()
            .map(|row| {
                Ok((
                    row.try_get::<i32, _>("version")?,
                    UnixMillis::from_millis(row.try_get::<i64, _>("created_at")?)?,
                ))
            })
            .collect()
    }

    /// Seals the secret of `request` under its cipher and stores the
    /// credential (inside a tenant-bound transaction, so row-level security
    /// also guards the write), returning its metadata record — which carries
    /// only the secret's fingerprint, never the secret.
    pub async fn store_credential(
        &self,
        request: NewCredential<'_>,
    ) -> StorageResult<CredentialRecord> {
        if request.id.trim().is_empty() {
            return Err(StorageError::Invalid {
                what: "credential id",
                reason: "must not be blank".to_owned(),
            });
        }
        let sealed = request.cipher.seal(request.secret.expose())?;
        let mut tx = self.begin_tenant(request.tenant).await?;
        sqlx::query(
            "INSERT INTO credentials
                 (id, tenant_id, platform, kind, secret_nonce, secret_cipher,
                  dek_nonce, wrapped_dek, kek_version, fingerprint, created_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)",
        )
        .bind(request.id)
        .bind(request.tenant.as_str())
        .bind(request.platform)
        .bind(request.kind)
        .bind(&sealed.data_nonce)
        .bind(&sealed.data_cipher)
        .bind(&sealed.dek_nonce)
        .bind(&sealed.wrapped_dek)
        .bind(sealed.kek_version)
        .bind(request.secret.fingerprint())
        .bind(request.created_at.as_millis())
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(CredentialRecord {
            id: request.id.to_owned(),
            tenant_id: request.tenant.clone(),
            platform: request.platform.to_owned(),
            kind: request.kind.to_owned(),
            kek_version: sealed.kek_version,
            fingerprint: Some(request.secret.fingerprint()),
            created_at: request.created_at,
            is_revoked: false,
            revoked_at: None,
        })
    }

    /// Loads the sealed envelope of one credential **of `tenant`** (RLS
    /// scoped), or [`StorageError::NotFound`]. Revoked credentials are
    /// refused here: they must never be opened again.
    pub async fn load_credential(
        &self,
        tenant: &TenantId,
        id: &str,
    ) -> StorageResult<SealedSecret> {
        let mut tx = self.begin_tenant(tenant).await?;
        let row = sqlx::query(
            "SELECT secret_nonce, secret_cipher, dek_nonce, wrapped_dek, kek_version,
                    is_revoked
             FROM credentials WHERE id = $1",
        )
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?;
        tx.commit().await?;
        let Some(row) = row else {
            return Err(StorageError::NotFound {
                what: "credential",
                id: id.to_owned(),
            });
        };
        if row.try_get::<bool, _>("is_revoked")? {
            return Err(StorageError::PermissionDenied {
                tenant: tenant.as_str().to_owned(),
                role: "credential".to_owned(),
                scope: "revoked".to_owned(),
            });
        }
        Ok(SealedSecret {
            kek_version: row.try_get::<i32, _>("kek_version")?,
            dek_nonce: row.try_get::<Vec<u8>, _>("dek_nonce")?,
            wrapped_dek: row.try_get::<Vec<u8>, _>("wrapped_dek")?,
            data_nonce: row.try_get::<Vec<u8>, _>("secret_nonce")?,
            data_cipher: row.try_get::<Vec<u8>, _>("secret_cipher")?,
        })
    }

    /// Rewrites a credential's envelope in place (used by rotation);
    /// reports whether a live credential was updated.
    pub async fn replace_credential_envelope(
        &self,
        tenant: &TenantId,
        id: &str,
        sealed: &SealedSecret,
    ) -> StorageResult<bool> {
        let mut tx = self.begin_tenant(tenant).await?;
        let result = sqlx::query(
            "UPDATE credentials
             SET secret_nonce = $2, secret_cipher = $3, dek_nonce = $4,
                 wrapped_dek = $5, kek_version = $6
             WHERE id = $1 AND NOT is_revoked",
        )
        .bind(id)
        .bind(&sealed.data_nonce)
        .bind(&sealed.data_cipher)
        .bind(&sealed.dek_nonce)
        .bind(&sealed.wrapped_dek)
        .bind(sealed.kek_version)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(result.rows_affected() == 1)
    }

    /// Revokes a credential; reports whether a live credential was
    /// revoked, so the call is idempotent. Revoked credentials can no
    /// longer be opened ([`Self::load_credential`] refuses them).
    pub async fn revoke_credential(
        &self,
        tenant: &TenantId,
        id: &str,
        now: UnixMillis,
    ) -> StorageResult<bool> {
        let mut tx = self.begin_tenant(tenant).await?;
        let row = sqlx::query(
            "UPDATE credentials SET is_revoked = TRUE, revoked_at = $2
             WHERE id = $1 AND NOT is_revoked
             RETURNING id",
        )
        .bind(id)
        .bind(now.as_millis())
        .fetch_optional(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(row.is_some())
    }

    /// Revokes a credential and, in the same transaction, revokes every
    /// `AUTHORIZED` session that carries it (F040): a dead carrier must
    /// not keep live sessions authorized — the domain machine has no
    /// re-auth state, so the sessions end as `REVOKED` with an explicit
    /// note. Idempotent: revoking an already-revoked credential reports
    /// `revoked: false` with its original instant and touches nothing.
    /// A missing (or foreign) credential is [`StorageError::NotFound`].
    pub async fn revoke_credential_with_sessions(
        &self,
        tenant: &TenantId,
        id: &str,
        now: UnixMillis,
    ) -> StorageResult<CredentialRevocation> {
        let mut tx = self.begin_tenant(tenant).await?;
        let revoked_now = sqlx::query(
            "UPDATE credentials SET is_revoked = TRUE, revoked_at = $2
             WHERE id = $1 AND NOT is_revoked",
        )
        .bind(id)
        .bind(now.as_millis())
        .execute(&mut *tx)
        .await?
        .rows_affected()
            == 1;
        let revoked_at = if revoked_now {
            now
        } else {
            let row = sqlx::query("SELECT revoked_at FROM credentials WHERE id = $1")
                .bind(id)
                .fetch_optional(&mut *tx)
                .await?;
            let Some(row) = row else {
                return Err(StorageError::NotFound {
                    what: "credential",
                    id: id.to_owned(),
                });
            };
            UnixMillis::from_millis(row.try_get::<i64, _>("revoked_at")?)?
        };
        let revoked_sessions = if revoked_now {
            sqlx::query(
                "UPDATE auth_sessions
                 SET state = 'REVOKED', state_note = 'credential revoked', updated_at = $3
                 WHERE tenant_id = $1 AND credential_id = $2 AND state = 'AUTHORIZED'",
            )
            .bind(tenant.as_str())
            .bind(id)
            .bind(now.as_millis())
            .execute(&mut *tx)
            .await?
            .rows_affected()
        } else {
            0
        };
        tx.commit().await?;
        Ok(CredentialRevocation {
            credential: id.to_owned(),
            revoked: revoked_now,
            revoked_at,
            revoked_sessions,
        })
    }

    /// Lists a tenant's credential metadata (no secret material).
    pub async fn list_credentials(
        &self,
        tenant: &TenantId,
    ) -> StorageResult<Vec<CredentialRecord>> {
        let mut tx = self.begin_tenant(tenant).await?;
        let rows = sqlx::query(
            "SELECT id, tenant_id, platform, kind, kek_version, created_at,
                    is_revoked, revoked_at
             FROM credentials
             ORDER BY created_at, id",
        )
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;
        rows.iter()
            .map(|row| {
                let revoked_at: Option<i64> = row.try_get("revoked_at")?;
                Ok(CredentialRecord {
                    id: row.try_get::<String, _>("id")?,
                    tenant_id: TenantId::parse(row.try_get::<String, _>("tenant_id")?)?,
                    platform: row.try_get::<String, _>("platform")?,
                    kind: row.try_get::<String, _>("kind")?,
                    kek_version: row.try_get::<i32, _>("kek_version")?,
                    fingerprint: None,
                    created_at: UnixMillis::from_millis(row.try_get::<i64, _>("created_at")?)?,
                    is_revoked: row.try_get::<bool, _>("is_revoked")?,
                    revoked_at: revoked_at.map(UnixMillis::from_millis).transpose()?,
                })
            })
            .collect()
    }

    /// Retires a master-key generation after rotation completed (the
    /// generation's ciphertext must have been re-wrapped first; retirement
    /// is bookkeeping, not enforcement).
    pub async fn retire_master_key(&self, version: i32) -> StorageResult<bool> {
        let result = sqlx::query(
            "UPDATE master_keys SET is_retired = TRUE
             WHERE version = $1 AND NOT is_retired",
        )
        .bind(version)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() == 1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cipher(version: i32) -> CredentialCipher {
        CredentialCipher::new(MasterKey::generate(), version)
    }

    #[test]
    fn seal_open_roundtrip_preserves_the_secret() {
        let seal = cipher(1);
        let secret = "token-abc-123 with spaces & symbols ünïcode";
        let sealed = seal.seal(secret).expect("seal");

        // Two envelopes of the same secret differ: fresh DEK and nonces.
        let again = seal.seal(secret).expect("seal again");
        assert_ne!(sealed, again, "sealing must be randomized");
        assert_ne!(sealed.wrapped_dek, again.wrapped_dek);

        let opened = seal.open(&sealed).expect("open");
        assert_eq!(opened.expose(), secret);
    }

    #[test]
    fn wrong_master_key_cannot_open() {
        let sealed = cipher(1).seal("the secret").expect("seal");
        let err = cipher(1).open(&sealed).expect_err("wrong key must fail");
        let StorageError::Invalid { what, reason } = err else {
            panic!("expected Invalid, got {err:?}")
        };
        assert_eq!(what, "sealed secret");
        // The failure message must not echo the secret.
        assert!(!reason.contains("the secret"));
    }

    #[test]
    fn tampered_ciphertext_is_rejected() {
        let seal = cipher(7);
        let mut sealed = seal.seal("the secret").expect("seal");
        let last = sealed.data_cipher.len() - 1;
        sealed.data_cipher[last] ^= 0x01;
        let err = seal.open(&sealed).expect_err("tampered data must fail");
        assert!(matches!(err, StorageError::Invalid { .. }), "got {err:?}");

        // Tampering with the wrapped DEK fails the same way.
        let sealed = seal.seal("the secret").expect("seal again");
        let mut sealed = sealed;
        sealed.wrapped_dek[0] ^= 0x01;
        assert!(seal.open(&sealed).is_err(), "tampered DEK wrap must fail");
    }

    #[test]
    fn kek_rotation_preserves_the_secret_and_bumps_the_version() {
        let old = cipher(1);
        let sealed = old.seal("rotate me").expect("seal");

        let new = CredentialCipher::new(MasterKey::generate(), 2);
        let rewrapped = old.rewrap(&sealed, &new).expect("rewrap");

        // Rotation moves only the wrap.
        assert_eq!(rewrapped.kek_version, 2);
        assert_eq!(rewrapped.data_cipher, sealed.data_cipher);
        assert_eq!(rewrapped.data_nonce, sealed.data_nonce);
        assert_ne!(rewrapped.wrapped_dek, sealed.wrapped_dek);

        // New generation opens it, the old one no longer can.
        assert_eq!(new.open(&rewrapped).expect("open").expose(), "rotate me");
        assert!(old.open(&rewrapped).is_err(), "old key must not open");

        // The original envelope is still readable by the old generation.
        assert_eq!(old.open(&sealed).expect("open").expose(), "rotate me");
    }

    #[test]
    fn debug_renderings_and_errors_never_leak_material() {
        let secret = "fp_plain_secret_value_never_leak_42";
        let seal = cipher(1);
        let sealed = seal.seal(secret).expect("seal");

        assert!(!format!("{sealed:?}").contains(secret));
        let opened = seal.open(&sealed).expect("open");
        assert!(!format!("{opened:?}").contains(secret));
        assert!(format!("{opened:?}").contains(&opened.fingerprint()));
        assert!(!format!("{seal:?}").contains(&format!("{:?}", seal.kek.0)));

        let wrong = cipher(1);
        let mut sealed = sealed;
        sealed.wrapped_dek[0] ^= 0x01;
        let err = wrong.open(&sealed).expect_err("must fail");
        assert!(!err.to_string().contains(secret));
    }

    #[test]
    fn master_keys_parse_strict_hex_and_reject_bad_input() {
        let key = MasterKey::generate();
        let hex = key
            .0
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<Vec<_>>()
            .join("");
        let reparsed = MasterKey::from_hex(&hex).expect("valid hex round-trips");
        assert_eq!(reparsed.fingerprint(), key.fingerprint());

        for bad in ["", "zz", "abc", "0", &"a".repeat(63), &"a".repeat(65)] {
            assert!(
                MasterKey::from_hex(bad).is_err(),
                "{bad:?} must not parse as a master key"
            );
        }
    }

    #[test]
    fn missing_env_var_is_reported_not_panicked() {
        // An unlikely-to-be-set name keeps the test hermetic.
        let err = MasterKey::from_env("FISHPOND_TEST_MISSING_MASTER_KEY")
            .expect_err("unset var must error");
        assert!(matches!(err, StorageError::Invalid { .. }), "got {err:?}");
    }
}
