//! F034: short-lived operator takeover grants (T10, manual takeover).
//!
//! A grant is the operator's credential for driving a taken-over
//! session remotely. The raw token exists only in the issue response;
//! the database holds the SHA-256 hash and a display tail, and
//! verification resolves the grant by token possession alone (the
//! auth-time path, before any tenant is known). Administration
//! (list/revoke/live lookup) is tenant-scoped by explicit predicates
//! — this table carries no RLS, mirroring `api_keys`.

use aes_gcm::aead::OsRng;
use rand_core::RngCore;
use sha2::{Digest, Sha256};
use sqlx::Row;

use fishpond_domain::{TenantId, UnixMillis};

use crate::error::{StorageError, StorageResult};
use crate::store::Store;

/// Every issued token starts with this prefix, so a leaked string is
/// recognizable as a takeover credential.
pub const TOKEN_PREFIX: &str = "fp_tk_";

/// A fresh high-entropy token: 32 CSPRNG bytes, hex, prefixed. Raw
/// material lives only in this return value.
pub fn generate_takeover_token() -> String {
    let mut bytes = [0u8; 32];
    OsRng.fill_bytes(&mut bytes);
    let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    format!("{TOKEN_PREFIX}{hex}")
}

/// SHA-256 hex of a raw token. Tokens are high-entropy random strings,
/// so (like API keys) they are hashed without a salt.
pub fn hash_takeover_token(raw: &str) -> String {
    let digest = Sha256::digest(raw.as_bytes());
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// The display tail of a raw token (never the full token).
fn token_hint(raw: &str) -> String {
    raw.chars()
        .rev()
        .take(4)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect()
}

/// Input for [`Store::issue_takeover_grant`]. `token` is the *raw*
/// token; only its hash and hint are stored.
pub struct NewTakeoverGrant<'a> {
    /// Grant id (primary key, caller-minted).
    pub id: &'a str,
    /// Owning tenant.
    pub tenant: &'a TenantId,
    /// The taken-over account.
    pub account: &'a str,
    /// The authorization session being driven.
    pub session: &'a str,
    /// Who the credential is for (audit display, not a secret).
    pub operator: &'a str,
    /// The *raw* token — hashed before it reaches the database.
    pub token: &'a str,
    /// Issue instant.
    pub created_at: UnixMillis,
    /// The short lifetime's end; verification dies here.
    pub expires_at: UnixMillis,
}

/// A stored grant. `token_hash` is all the secret-facing surface
/// there is; the raw token is unrecoverable from it.
#[derive(Clone, Debug)]
pub struct TakeoverGrantRecord {
    /// Grant id.
    pub id: String,
    /// Owning tenant.
    pub tenant_id: String,
    /// The taken-over account.
    pub account_id: String,
    /// The authorization session being driven.
    pub session_id: String,
    /// Who the credential is for.
    pub operator: String,
    /// SHA-256 hex of the raw token.
    pub token_hash: String,
    /// The raw token's display tail.
    pub token_hint: String,
    /// Issue instant.
    pub created_at: UnixMillis,
    /// The short lifetime's end.
    pub expires_at: UnixMillis,
    /// When the grant was revoked, if it was.
    pub revoked_at: Option<UnixMillis>,
}

const GRANT_COLUMNS: &str = "id, tenant_id, account_id, session_id, operator,
     token_hash, token_hint, created_at, expires_at, revoked_at";

fn grant_from_row(row: &sqlx::postgres::PgRow) -> StorageResult<TakeoverGrantRecord> {
    Ok(TakeoverGrantRecord {
        id: row.try_get("id")?,
        tenant_id: row.try_get("tenant_id")?,
        account_id: row.try_get("account_id")?,
        session_id: row.try_get("session_id")?,
        operator: row.try_get("operator")?,
        token_hash: row.try_get("token_hash")?,
        token_hint: row.try_get("token_hint")?,
        created_at: UnixMillis::from_millis(row.try_get("created_at")?).map_err(|_| {
            StorageError::Invalid {
                what: "takeover grant",
                reason: "stored created_at is negative".to_owned(),
            }
        })?,
        expires_at: UnixMillis::from_millis(row.try_get("expires_at")?).map_err(|_| {
            StorageError::Invalid {
                what: "takeover grant",
                reason: "stored expires_at is negative".to_owned(),
            }
        })?,
        revoked_at: row
            .try_get::<Option<i64>, _>("revoked_at")?
            .map(UnixMillis::from_millis)
            .transpose()
            .map_err(|_| StorageError::Invalid {
                what: "takeover grant",
                reason: "stored revoked_at is negative".to_owned(),
            })?,
    })
}

impl Store {
    /// Issues a grant: the operator's short-lived credential to drive
    /// the taken-over session. Only one live grant per session exists
    /// (schema-enforced); a second issue is a named conflict until the
    /// first is revoked. Revoked and expired rows stay for audit.
    pub async fn issue_takeover_grant(
        &self,
        grant: &NewTakeoverGrant<'_>,
    ) -> StorageResult<TakeoverGrantRecord> {
        if grant.id.trim().is_empty() {
            return Err(StorageError::Invalid {
                what: "takeover grant",
                reason: "grant id must not be blank".to_owned(),
            });
        }
        if grant.expires_at <= grant.created_at {
            return Err(StorageError::Invalid {
                what: "takeover grant",
                reason: "a short-lived credential must expire after it is issued".to_owned(),
            });
        }
        let mut tx = self.begin_tenant(grant.tenant).await?;
        let inserted = sqlx::query(
            "INSERT INTO takeover_grants
                 (id, tenant_id, account_id, session_id, operator,
                  token_hash, token_hint, created_at, expires_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
        )
        .bind(grant.id)
        .bind(grant.tenant.as_str())
        .bind(grant.account)
        .bind(grant.session)
        .bind(grant.operator)
        .bind(hash_takeover_token(grant.token))
        .bind(token_hint(grant.token))
        .bind(grant.created_at.as_millis())
        .bind(grant.expires_at.as_millis())
        .execute(&mut *tx)
        .await;
        if let Err(error) = inserted {
            if let Some(constraint) = error
                .as_database_error()
                .and_then(|database| database.constraint())
            {
                if constraint == "uq_takeover_grants_live_session" {
                    return Err(StorageError::Conflict {
                        what: "takeover grant",
                        id: grant.session.to_owned(),
                        reason: "session already has a live grant".to_owned(),
                    });
                }
                if constraint == "takeover_grants_token_hash_key" {
                    return Err(StorageError::Conflict {
                        what: "takeover grant",
                        id: grant.id.to_owned(),
                        reason: "token hash already issued".to_owned(),
                    });
                }
            }
            return Err(error.into());
        }
        tx.commit().await?;
        Ok(TakeoverGrantRecord {
            id: grant.id.to_owned(),
            tenant_id: grant.tenant.as_str().to_owned(),
            account_id: grant.account.to_owned(),
            session_id: grant.session.to_owned(),
            operator: grant.operator.to_owned(),
            token_hash: hash_takeover_token(grant.token),
            token_hint: token_hint(grant.token),
            created_at: grant.created_at,
            expires_at: grant.expires_at,
            revoked_at: None,
        })
    }

    /// Verifies a raw token: the auth-time path (no tenant context —
    /// the operator presents possession alone). Live means unrevoked
    /// and unexpired at `now`; anything else verifies to nothing.
    pub async fn verify_takeover_token(
        &self,
        token: &str,
        now: UnixMillis,
    ) -> StorageResult<Option<TakeoverGrantRecord>> {
        let row = sqlx::query(&format!(
            "SELECT {GRANT_COLUMNS} FROM takeover_grants
             WHERE token_hash = $1
               AND revoked_at IS NULL
               AND expires_at > $2"
        ))
        .bind(hash_takeover_token(token))
        .bind(now.as_millis())
        .fetch_optional(self.pool())
        .await?;
        match row {
            Some(row) => grant_from_row(&row).map(Some),
            None => Ok(None),
        }
    }

    /// The live (unrevoked, unexpired at `now`) grant of `session`,
    /// visible only to its tenant.
    pub async fn live_takeover_grant(
        &self,
        tenant: &TenantId,
        session_id: &str,
        now: UnixMillis,
    ) -> StorageResult<Option<TakeoverGrantRecord>> {
        let mut tx = self.begin_tenant(tenant).await?;
        let row = sqlx::query(&format!(
            "SELECT {GRANT_COLUMNS} FROM takeover_grants
             WHERE tenant_id = $1 AND session_id = $2
               AND revoked_at IS NULL AND expires_at > $3"
        ))
        .bind(tenant.as_str())
        .bind(session_id)
        .bind(now.as_millis())
        .fetch_optional(&mut *tx)
        .await?;
        tx.commit().await?;
        match row {
            Some(row) => grant_from_row(&row).map(Some),
            None => Ok(None),
        }
    }

    /// Every grant ever issued to `session`, oldest first — the audit
    /// surface. Tenant-scoped.
    pub async fn list_takeover_grants(
        &self,
        tenant: &TenantId,
        session_id: &str,
    ) -> StorageResult<Vec<TakeoverGrantRecord>> {
        let mut tx = self.begin_tenant(tenant).await?;
        let rows = sqlx::query(&format!(
            "SELECT {GRANT_COLUMNS} FROM takeover_grants
             WHERE tenant_id = $1 AND session_id = $2
             ORDER BY created_at, id"
        ))
        .bind(tenant.as_str())
        .bind(session_id)
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;
        rows.iter().map(grant_from_row).collect()
    }

    /// Revokes `grant_id` (release face): the token stops verifying at
    /// once. Reports whether a live grant was revoked; a foreign
    /// tenant's revoke is a no-op, not an error.
    pub async fn revoke_takeover_grant(
        &self,
        tenant: &TenantId,
        grant_id: &str,
        at: UnixMillis,
    ) -> StorageResult<bool> {
        let mut tx = self.begin_tenant(tenant).await?;
        let result = sqlx::query(
            "UPDATE takeover_grants SET revoked_at = $3
             WHERE tenant_id = $1 AND id = $2 AND revoked_at IS NULL",
        )
        .bind(tenant.as_str())
        .bind(grant_id)
        .bind(at.as_millis())
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(result.rows_affected() == 1)
    }
}
