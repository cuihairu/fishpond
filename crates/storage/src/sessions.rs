//! Authorization-session persistence (F009): one row per account login
//! flow, tenant-scoped like every other repository.
//!
//! The state machine itself lives in [`fishpond_domain::AuthSessionState`];
//! this module only stores and fetches rows — enforcing transitions and
//! coordination rules (one live session per account, credential version
//! checks) is the service layer's job (`fishpond-server`, F009/F011).

use fishpond_domain::{AccountId, AuthSessionId, AuthSessionState, TenantId, UnixMillis};
use sqlx::Row;

use crate::error::{StorageError, StorageResult};
use crate::store::Store;

/// One authorization-session row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthSessionRecord {
    /// Session id.
    pub id: AuthSessionId,
    /// Owning tenant.
    pub tenant_id: TenantId,
    /// Account the session logs in.
    pub account_id: AccountId,
    /// Platform the login happens on (informational; equals the account's).
    pub platform: String,
    /// Session state machine state.
    pub state: AuthSessionState,
    /// Human-readable qualifier for the state, if any.
    pub state_note: Option<String>,
    /// Credential bound at authorize time, if any.
    pub credential_id: Option<String>,
    /// Master-key generation that encrypted the bound credential's
    /// envelope, if any — the version pin F011 validates against.
    pub credential_version: Option<i32>,
    /// Creation instant.
    pub created_at: UnixMillis,
    /// Instant of the last state change.
    pub updated_at: UnixMillis,
    /// Instant after which the session must no longer be used, if any.
    pub expires_at: Option<UnixMillis>,
}

/// Everything mutable on a session row; `None` fields leave the column
/// untouched.
#[derive(Debug, Clone, Default)]
pub struct AuthSessionUpdate {
    /// New state (callers enforce the machine; storage persists it).
    pub state: Option<AuthSessionState>,
    /// New state note (`None` leaves it; use `Some(None)` semantics via
    /// [`Self::clear_note`]).
    pub state_note: Option<String>,
    /// Whether to clear the state note.
    pub clear_note: bool,
    /// Credential to bind (authorize).
    pub credential_id: Option<String>,
    /// Credential envelope version to pin (authorize).
    pub credential_version: Option<i32>,
    /// New expiry instant.
    pub expires_at: Option<UnixMillis>,
}

impl AuthSessionUpdate {
    /// A state-only update.
    pub fn state(state: AuthSessionState) -> Self {
        Self {
            state: Some(state),
            ..Self::default()
        }
    }
}

/// Insert/update payload for a new session.
#[derive(Debug, Clone)]
pub struct NewAuthSession {
    /// Session id.
    pub id: AuthSessionId,
    /// Owning tenant.
    pub tenant: TenantId,
    /// Account being logged in.
    pub account_id: AccountId,
    /// Platform (informational).
    pub platform: String,
    /// Initial state (normally [`AuthSessionState::Pending`]).
    pub state: AuthSessionState,
    /// Creation instant (also the initial `updated_at`).
    pub created_at: UnixMillis,
    /// Login TTL, if the platform enforces one.
    pub expires_at: Option<UnixMillis>,
}

impl Store {
    /// Inserts one session row; fails with a database error on duplicate
    /// session ids.
    pub async fn insert_auth_session(&self, session: &NewAuthSession) -> StorageResult<()> {
        let mut tx = self.begin_tenant(&session.tenant).await?;
        sqlx::query(
            "INSERT INTO auth_sessions
                 (id, tenant_id, account_id, platform, state, created_at, updated_at, expires_at)
             VALUES ($1, $2, $3, $4, $5, $6, $6, $7)",
        )
        .bind(session.id.as_str())
        .bind(session.tenant.as_str())
        .bind(session.account_id.as_str())
        .bind(&session.platform)
        .bind(session.state.as_str())
        .bind(session.created_at.as_millis())
        .bind(session.expires_at.map(UnixMillis::as_millis))
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Returns one session **of `tenant`** (RLS scoped), or
    /// [`StorageError::NotFound`].
    pub async fn get_auth_session(
        &self,
        tenant: &TenantId,
        id: &AuthSessionId,
    ) -> StorageResult<AuthSessionRecord> {
        let mut tx = self.begin_tenant(tenant).await?;
        let row = sqlx::query(
            "SELECT id, tenant_id, account_id, platform, state, state_note,
                    credential_id, credential_version, created_at, updated_at, expires_at
             FROM auth_sessions WHERE id = $1",
        )
        .bind(id.as_str())
        .fetch_optional(&mut *tx)
        .await?;
        tx.commit().await?;
        match row {
            Some(row) => Ok(Self::map_auth_session(&row)?),
            None => Err(StorageError::NotFound {
                what: "auth session",
                id: id.as_str().to_owned(),
            }),
        }
    }

    /// Lists sessions of `tenant`, optionally narrowed to one account,
    /// oldest first.
    pub async fn list_auth_sessions(
        &self,
        tenant: &TenantId,
        account: Option<&AccountId>,
    ) -> StorageResult<Vec<AuthSessionRecord>> {
        let mut tx = self.begin_tenant(tenant).await?;
        let rows = sqlx::query(
            "SELECT id, tenant_id, account_id, platform, state, state_note,
                    credential_id, credential_version, created_at, updated_at, expires_at
             FROM auth_sessions
             WHERE ($1::text IS NULL OR account_id = $1)
             ORDER BY created_at, id",
        )
        .bind(account.map(AccountId::as_str))
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;
        rows.iter().map(Self::map_auth_session).collect()
    }

    /// Applies a partial update to one session **of `tenant`**; reports
    /// whether a row changed. Terminal-state rows are still writable by
    /// design: the *machine* is the service's responsibility, storage only
    /// records what the service decided.
    pub async fn update_auth_session(
        &self,
        tenant: &TenantId,
        id: &AuthSessionId,
        update: &AuthSessionUpdate,
        at: UnixMillis,
    ) -> StorageResult<bool> {
        let mut tx = self.begin_tenant(tenant).await?;
        let result = sqlx::query(
            "UPDATE auth_sessions SET
                 state = COALESCE($3, state),
                 state_note = CASE WHEN $5 THEN NULL ELSE COALESCE($4, state_note) END,
                 credential_id = COALESCE($6, credential_id),
                 credential_version = COALESCE($7, credential_version),
                 expires_at = COALESCE($8, expires_at),
                 updated_at = $2
             WHERE id = $1",
        )
        .bind(id.as_str())
        .bind(at.as_millis())
        .bind(update.state.map(|state| state.as_str()))
        .bind(update.state_note.clone())
        .bind(update.clear_note)
        .bind(update.credential_id.clone())
        .bind(update.credential_version)
        .bind(update.expires_at.map(UnixMillis::as_millis))
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(result.rows_affected() == 1)
    }

    /// Deletes one session row (used when a session record is purged);
    /// reports whether a row was removed.
    pub async fn delete_auth_session(
        &self,
        tenant: &TenantId,
        id: &AuthSessionId,
    ) -> StorageResult<bool> {
        let mut tx = self.begin_tenant(tenant).await?;
        let result = sqlx::query("DELETE FROM auth_sessions WHERE id = $1")
            .bind(id.as_str())
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(result.rows_affected() == 1)
    }

    fn map_auth_session(row: &sqlx::postgres::PgRow) -> StorageResult<AuthSessionRecord> {
        let state_raw: String = row.try_get("state")?;
        let state = AuthSessionState::parse(&state_raw).map_err(|_| StorageError::Invalid {
            what: "auth session row",
            reason: format!("unknown state {state_raw:?}"),
        })?;
        Ok(AuthSessionRecord {
            id: AuthSessionId::parse(row.try_get::<String, _>("id")?)?,
            tenant_id: TenantId::parse(row.try_get::<String, _>("tenant_id")?)?,
            account_id: AccountId::parse(row.try_get::<String, _>("account_id")?)?,
            platform: row.try_get("platform")?,
            state,
            state_note: row.try_get("state_note")?,
            credential_id: row.try_get("credential_id")?,
            credential_version: row.try_get("credential_version")?,
            created_at: UnixMillis::from_millis(row.try_get::<i64, _>("created_at")?)?,
            updated_at: UnixMillis::from_millis(row.try_get::<i64, _>("updated_at")?)?,
            expires_at: row
                .try_get::<Option<i64>, _>("expires_at")?
                .map(UnixMillis::from_millis)
                .transpose()?,
        })
    }
}
