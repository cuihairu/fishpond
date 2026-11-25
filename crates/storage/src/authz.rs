//! API-key authorization: roles, scopes and issue/resolve/revoke (F007).
//!
//! Keys are SHA-256 hashed before they reach the database, so raw key
//! material is never stored — only a short tail hint for audit screens.
//! RBAC: a named role carries a scope bundle and a key **freezes** its
//! role's scopes at issue time, so editing a role later can never
//! retroactively grant or revoke a live key. Resolution returns `None`
//! for unknown, revoked and expired keys alike — presented credentials must
//! not become an oracle.
//!
//! The scope names mirror the MCP facade (`crates/mcp/src/auth.rs`) so the
//! M0 in-memory key store and this persistent store stay interchangeable.
//!
//! `tenant_roles` and `api_keys` carry no row-level security: a key has to
//! be resolvable *before* its tenant is known, and the table holds hashes
//! and public metadata only. All tenant-owned tables are RLS-isolated
//! instead (migration 0002, [`Store::begin_tenant`]).

use fishpond_domain::{TenantId, UnixMillis};
use sha2::{Digest, Sha256};
use sqlx::Row;

use crate::error::{StorageError, StorageResult};
use crate::store::Store;

/// Read accounts.
pub const SCOPE_ACCOUNTS_READ: &str = "accounts:read";
/// Read conversations.
pub const SCOPE_CONVERSATIONS_READ: &str = "conversations:read";
/// Send messages.
pub const SCOPE_CONVERSATIONS_WRITE: &str = "conversations:write";
/// Read the product catalog.
pub const SCOPE_CATALOG_READ: &str = "catalog:read";
/// Read operations.
pub const SCOPE_OPERATIONS_READ: &str = "operations:read";
/// Read events.
pub const SCOPE_EVENTS_READ: &str = "events:read";

/// Every well-known scope (mirrors the MCP facade's `Scope::ALL`).
pub const SCOPE_ALL: [&str; 6] = [
    SCOPE_ACCOUNTS_READ,
    SCOPE_CONVERSATIONS_READ,
    SCOPE_CONVERSATIONS_WRITE,
    SCOPE_CATALOG_READ,
    SCOPE_OPERATIONS_READ,
    SCOPE_EVENTS_READ,
];

/// A named role: a reusable bundle of scopes (RBAC seed of migration 0002).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoleRecord {
    /// Role name (primary key in `tenant_roles`).
    pub name: String,
    /// The scopes the role grants.
    pub scopes: Vec<String>,
}

/// A stored API key. Only the hash and the display tail ever exist here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApiKeyRecord {
    /// SHA-256 hex of the raw key.
    pub key_hash: String,
    /// Last characters, for audit display.
    pub key_hint: String,
    /// The tenant the key authenticates as.
    pub tenant_id: TenantId,
    /// The role it was issued under.
    pub role: String,
    /// Scopes frozen from that role at issue time.
    pub scopes: Vec<String>,
    /// Issue instant.
    pub created_at: UnixMillis,
    /// Expiry instant (a key stops resolving at or after this instant).
    pub expires_at: UnixMillis,
    /// Whether it was revoked.
    pub is_revoked: bool,
    /// Revocation instant, if revoked.
    pub revoked_at: Option<UnixMillis>,
}

/// A presented key that authenticated: tenant + role + frozen scopes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedKey {
    /// The tenant the key authenticates as.
    pub tenant_id: TenantId,
    /// The role it was issued under.
    pub role: String,
    /// Scopes the call is allowed to use.
    pub scopes: Vec<String>,
    /// Expiry instant.
    pub expires_at: UnixMillis,
}

impl ResolvedKey {
    /// Whether the key may use `scope`.
    pub fn has_scope(&self, scope: &str) -> bool {
        self.scopes.iter().any(|s| s == scope)
    }
}

/// Enforces `scope` for an authenticated key, or fails with
/// [`StorageError::PermissionDenied`] naming tenant, role and scope — never
/// the key itself.
pub fn require_scope(key: &ResolvedKey, scope: &str) -> StorageResult<()> {
    if key.has_scope(scope) {
        Ok(())
    } else {
        Err(StorageError::PermissionDenied {
            tenant: key.tenant_id.as_str().to_owned(),
            role: key.role.clone(),
            scope: scope.to_owned(),
        })
    }
}

/// SHA-256 hex of a raw key. API keys are high-entropy random strings, so
/// (unlike passwords) they are hashed without a salt.
pub fn hash_api_key(raw: &str) -> String {
    let digest = Sha256::digest(raw.as_bytes());
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// The display tail of a raw key (never the full key).
fn key_hint(raw: &str) -> String {
    raw.chars()
        .rev()
        .take(4)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect()
}

impl Store {
    /// Upserts a role and its scope bundle (idempotent RBAC seed/patch).
    pub async fn upsert_role(&self, name: &str, scopes: &[&str]) -> StorageResult<()> {
        if name.trim().is_empty() {
            return Err(StorageError::Invalid {
                what: "role name",
                reason: "must not be blank".to_owned(),
            });
        }
        let scopes: Vec<String> = scopes.iter().map(|s| (*s).to_owned()).collect();
        sqlx::query(
            "INSERT INTO tenant_roles (name, scopes) VALUES ($1, $2)
             ON CONFLICT (name) DO UPDATE SET scopes = EXCLUDED.scopes",
        )
        .bind(name)
        .bind(scopes)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Lists every known role, ordered by name.
    pub async fn list_roles(&self) -> StorageResult<Vec<RoleRecord>> {
        let rows = sqlx::query("SELECT name, scopes FROM tenant_roles ORDER BY name")
            .fetch_all(&self.pool)
            .await?;
        rows.iter()
            .map(|row| {
                Ok(RoleRecord {
                    name: row.try_get::<String, _>("name")?,
                    scopes: row.try_get::<Vec<String>, _>("scopes")?,
                })
            })
            .collect()
    }

    /// Issues a key for `tenant` under `role`, freezing the role's scopes
    /// at that moment, and returns the stored record.
    pub async fn issue_api_key(
        &self,
        raw: &str,
        tenant: &TenantId,
        role: &str,
        created_at: UnixMillis,
        expires_at: UnixMillis,
    ) -> StorageResult<ApiKeyRecord> {
        if raw.trim().is_empty() {
            return Err(StorageError::Invalid {
                what: "api key",
                reason: "must not be blank".to_owned(),
            });
        }
        let mut tx = self.pool.begin().await?;
        let scopes: Vec<String> =
            sqlx::query_scalar::<_, Vec<String>>("SELECT scopes FROM tenant_roles WHERE name = $1")
                .bind(role)
                .fetch_optional(&mut *tx)
                .await?
                .ok_or_else(|| StorageError::NotFound {
                    what: "role",
                    id: role.to_owned(),
                })?;
        let key_hash = hash_api_key(raw);
        sqlx::query(
            "INSERT INTO api_keys (key_hash, key_hint, tenant_id, role, scopes, created_at, expires_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7)",
        )
        .bind(&key_hash)
        .bind(key_hint(raw))
        .bind(tenant.as_str())
        .bind(role)
        .bind(&scopes)
        .bind(created_at.as_millis())
        .bind(expires_at.as_millis())
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(ApiKeyRecord {
            key_hash,
            key_hint: key_hint(raw),
            tenant_id: tenant.clone(),
            role: role.to_owned(),
            scopes,
            created_at,
            expires_at,
            is_revoked: false,
            revoked_at: None,
        })
    }

    /// Lists a tenant's stored keys (hashes, never raw material).
    pub async fn list_api_keys(&self, tenant: &TenantId) -> StorageResult<Vec<ApiKeyRecord>> {
        let rows = sqlx::query(
            "SELECT key_hash, key_hint, tenant_id, role, scopes, created_at, expires_at,
                     is_revoked, revoked_at
             FROM api_keys WHERE tenant_id = $1
             ORDER BY created_at, key_hash",
        )
        .bind(tenant.as_str())
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(map_api_key).collect()
    }

    /// Resolves a presented key to tenant + role + frozen scopes; `None`
    /// when the key is unknown, revoked or expired.
    pub async fn resolve_api_key(
        &self,
        raw: &str,
        now: UnixMillis,
    ) -> StorageResult<Option<ResolvedKey>> {
        if raw.trim().is_empty() {
            return Ok(None);
        }
        let row = sqlx::query(
            "SELECT tenant_id, role, scopes, expires_at
             FROM api_keys
             WHERE key_hash = $1 AND NOT is_revoked AND expires_at > $2",
        )
        .bind(hash_api_key(raw))
        .bind(now.as_millis())
        .fetch_optional(&self.pool)
        .await?;
        let Some(row) = row else {
            return Ok(None);
        };
        Ok(Some(ResolvedKey {
            tenant_id: TenantId::parse(row.try_get::<String, _>("tenant_id")?)?,
            role: row.try_get::<String, _>("role")?,
            scopes: row.try_get::<Vec<String>, _>("scopes")?,
            expires_at: UnixMillis::from_millis(row.try_get::<i64, _>("expires_at")?)?,
        }))
    }

    /// Revokes a key by hash (the only thing the store knows about it);
    /// reports whether a live key was revoked, so the call is idempotent.
    pub async fn revoke_api_key(&self, key_hash: &str, now: UnixMillis) -> StorageResult<bool> {
        let row = sqlx::query(
            "UPDATE api_keys SET is_revoked = TRUE, revoked_at = $2
             WHERE key_hash = $1 AND NOT is_revoked
             RETURNING key_hash",
        )
        .bind(key_hash)
        .bind(now.as_millis())
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.is_some())
    }
}

/// Maps an api_keys row back into its record type.
fn map_api_key(row: &sqlx::postgres::PgRow) -> StorageResult<ApiKeyRecord> {
    let revoked_at: Option<i64> = row.try_get("revoked_at")?;
    Ok(ApiKeyRecord {
        key_hash: row.try_get::<String, _>("key_hash")?,
        key_hint: row.try_get::<String, _>("key_hint")?,
        tenant_id: TenantId::parse(row.try_get::<String, _>("tenant_id")?)?,
        role: row.try_get::<String, _>("role")?,
        scopes: row.try_get::<Vec<String>, _>("scopes")?,
        created_at: UnixMillis::from_millis(row.try_get::<i64, _>("created_at")?)?,
        expires_at: UnixMillis::from_millis(row.try_get::<i64, _>("expires_at")?)?,
        is_revoked: row.try_get::<bool, _>("is_revoked")?,
        revoked_at: revoked_at.map(UnixMillis::from_millis).transpose()?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn api_key_hashes_are_deterministic_hex_and_input_sensitive() {
        let first = hash_api_key("fpk-issued-key");
        assert_eq!(
            first,
            hash_api_key("fpk-issued-key"),
            "hashing is deterministic"
        );
        assert_eq!(first.len(), 64, "sha256 renders as 64 hex chars");
        assert!(
            first.chars().all(|c| c.is_ascii_hexdigit()),
            "expected hex, got {first}"
        );
        assert_ne!(
            hash_api_key("fpk-issued-key"),
            hash_api_key("fpk-issued-key-2"),
            "one changed character must change the hash"
        );
    }

    #[test]
    fn key_hints_show_only_the_tail() {
        assert_eq!(key_hint("fpk-abcdefghij"), "ghij");
        assert_eq!(key_hint("ab"), "ab");
        assert_eq!(key_hint(""), "");
    }

    #[test]
    fn scope_vocabulary_matches_the_mcp_facade() {
        // Mirrors crates/mcp/src/auth.rs, which the persistent store must
        // agree with so keys stay interchangeable between the two.
        assert_eq!(
            SCOPE_ALL,
            [
                "accounts:read",
                "conversations:read",
                "conversations:write",
                "catalog:read",
                "operations:read",
                "events:read"
            ]
        );
        for scope in SCOPE_ALL {
            assert!(
                !scope.is_empty() && scope.contains(':'),
                "{scope} is not a namespace-qualified scope"
            );
        }
    }
}
