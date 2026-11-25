//! Authorization sessions (F009): the login-flow service.
//!
//! A session is one attempt to log an account in on its platform. The
//! service drives the [`AuthSessionState`] machine: start (pending),
//! mark the platform-side confirmation step, authorize (binding the
//! credential that carries the login and its envelope version), then
//! expire / revoke / fail. Coordination rules that span sessions (one
//! live session per account, credential-version pinning) are the F011
//! [`crate::coordination::SessionCoordinator`]'s job — this module keeps
//! single-session transitions and their audit trail.

use crate::error::{ServiceError, ServiceResult};
use fishpond_domain::{AccountId, AuthSessionId, AuthSessionState, TenantId, UnixMillis};
use fishpond_storage::{AuthSessionRecord, AuthSessionUpdate, NewAuthSession};

/// The authorization-session service.
#[derive(Clone)]
pub struct Sessions {
    store: fishpond_storage::Store,
}

impl Sessions {
    /// Wraps an opened [`Store`].
    pub fn new(store: fishpond_storage::Store) -> Self {
        Self { store }
    }

    /// Starts a login flow for `account_id`: creates the session in
    /// `PENDING`. The account must exist and must not be disabled — a
    /// disabled account has no automation surface to log into.
    pub async fn start(
        &self,
        tenant: &TenantId,
        id: AuthSessionId,
        account_id: AccountId,
        at: UnixMillis,
        expires_at: Option<UnixMillis>,
    ) -> ServiceResult<AuthSessionRecord> {
        let account = self
            .store
            .get_account(tenant, &account_id)
            .await
            .map_err(|_| ServiceError::not_found("account", account_id.as_str()))?;
        if account.state == fishpond_domain::AccountState::Disabled {
            return Err(ServiceError::conflict(
                "auth session",
                id.as_str(),
                "account is disabled and cannot log in",
            ));
        }
        let session = NewAuthSession {
            id,
            tenant: tenant.clone(),
            account_id,
            platform: account.platform.clone(),
            state: AuthSessionState::Pending,
            created_at: at,
            expires_at,
        };
        self.store.insert_auth_session(&session).await?;
        self.store
            .get_auth_session(tenant, &session.id)
            .await
            .map_err(Into::into)
    }

    /// Marks the platform-side confirmation step (e.g. the user scanned
    /// the QR code but the callback has not arrived): `PENDING →
    /// AWAITING_CONFIRMATION`.
    pub async fn mark_confirmation(
        &self,
        tenant: &TenantId,
        id: &AuthSessionId,
        note: Option<String>,
        at: UnixMillis,
    ) -> ServiceResult<AuthSessionRecord> {
        self.transition(
            tenant,
            id,
            AuthSessionState::AwaitingConfirmation,
            AuthSessionUpdate {
                state_note: note,
                ..AuthSessionUpdate::default()
            },
            at,
        )
        .await
    }

    /// Authorizes the session: `→ AUTHORIZED`, binding `credential_id`
    /// and the envelope version it was opened with (`credential_version`).
    /// The pin is what lets F011 detect a later credential rotation. The
    /// credential must exist and not be revoked.
    pub async fn authorize(
        &self,
        tenant: &TenantId,
        id: &AuthSessionId,
        credential_id: &str,
        credential_version: i32,
        expires_at: Option<UnixMillis>,
        at: UnixMillis,
    ) -> ServiceResult<AuthSessionRecord> {
        // The credential row must be live: a revoked credential must not
        // become the carrier of a fresh login.
        self.store
            .load_credential(tenant, credential_id)
            .await
            .map_err(|_| ServiceError::not_found("credential", credential_id))?;
        self.transition(
            tenant,
            id,
            AuthSessionState::Authorized,
            AuthSessionUpdate {
                credential_id: Some(credential_id.to_owned()),
                credential_version: Some(credential_version),
                expires_at,
                ..AuthSessionUpdate::default()
            },
            at,
        )
        .await
    }

    /// Expires the session (login window elapsed or TTL reached).
    pub async fn expire(
        &self,
        tenant: &TenantId,
        id: &AuthSessionId,
        at: UnixMillis,
    ) -> ServiceResult<AuthSessionRecord> {
        self.transition(
            tenant,
            id,
            AuthSessionState::Expired,
            AuthSessionUpdate::default(),
            at,
        )
        .await
    }

    /// Revokes the session (operator or platform action).
    pub async fn revoke(
        &self,
        tenant: &TenantId,
        id: &AuthSessionId,
        note: Option<String>,
        at: UnixMillis,
    ) -> ServiceResult<AuthSessionRecord> {
        self.transition(
            tenant,
            id,
            AuthSessionState::Revoked,
            AuthSessionUpdate {
                state_note: note,
                ..AuthSessionUpdate::default()
            },
            at,
        )
        .await
    }

    /// Fails the login flow (platform rejected, callback error, ...).
    pub async fn fail(
        &self,
        tenant: &TenantId,
        id: &AuthSessionId,
        note: Option<String>,
        at: UnixMillis,
    ) -> ServiceResult<AuthSessionRecord> {
        self.transition(
            tenant,
            id,
            AuthSessionState::Failed,
            AuthSessionUpdate {
                state_note: note,
                ..AuthSessionUpdate::default()
            },
            at,
        )
        .await
    }

    /// Manual takeover (F011): `AUTHORIZED → TAKEN_OVER`. The machine
    /// rejects the transition from any other state — only a live login
    /// can be taken over.
    pub async fn take_over(
        &self,
        tenant: &TenantId,
        id: &AuthSessionId,
        note: Option<String>,
        at: UnixMillis,
    ) -> ServiceResult<AuthSessionRecord> {
        self.transition(
            tenant,
            id,
            AuthSessionState::TakenOver,
            AuthSessionUpdate {
                state_note: note,
                ..AuthSessionUpdate::default()
            },
            at,
        )
        .await
    }

    /// Resumes automation after a takeover (F011): `TAKEN_OVER →
    /// AUTHORIZED`. The credential pin and expiry are untouched — the
    /// coordinator re-checks the pin before the next automation step.
    pub async fn resume(
        &self,
        tenant: &TenantId,
        id: &AuthSessionId,
        at: UnixMillis,
    ) -> ServiceResult<AuthSessionRecord> {
        self.transition(
            tenant,
            id,
            AuthSessionState::Authorized,
            AuthSessionUpdate::default(),
            at,
        )
        .await
    }

    /// The envelope generation (`kek_version`) of a live credential —
    /// the "current" side of F011's pin comparison.
    pub async fn credential_version(
        &self,
        tenant: &TenantId,
        credential_id: &str,
    ) -> ServiceResult<i32> {
        let sealed = self
            .store
            .load_credential(tenant, credential_id)
            .await
            .map_err(|_| ServiceError::not_found("credential", credential_id))?;
        Ok(sealed.kek_version)
    }

    /// Returns one session of `tenant`, or [`ServiceError::NotFound`].
    pub async fn get(
        &self,
        tenant: &TenantId,
        id: &AuthSessionId,
    ) -> ServiceResult<AuthSessionRecord> {
        self.store
            .get_auth_session(tenant, id)
            .await
            .map_err(Into::into)
    }

    /// Lists sessions of `tenant`, optionally narrowed to one account.
    pub async fn list(
        &self,
        tenant: &TenantId,
        account: Option<&AccountId>,
    ) -> ServiceResult<Vec<AuthSessionRecord>> {
        self.store
            .list_auth_sessions(tenant, account)
            .await
            .map_err(Into::into)
    }

    /// Sessions that are usable right now (`AUTHORIZED` and not past
    /// their expiry) — the predicate automation checks before acting.
    pub async fn live_for_account(
        &self,
        tenant: &TenantId,
        account: &AccountId,
        now: UnixMillis,
    ) -> ServiceResult<Vec<AuthSessionRecord>> {
        let sessions = self.list(tenant, Some(account)).await?;
        Ok(sessions
            .into_iter()
            .filter(|session| {
                session.state == AuthSessionState::Authorized
                    && session.expires_at.is_none_or(|until| until > now)
            })
            .collect())
    }

    /// Loads the session, applies `next` through the domain machine and
    /// persists the update. Unknown sessions surface as `NotFound`.
    async fn transition(
        &self,
        tenant: &TenantId,
        id: &AuthSessionId,
        next: AuthSessionState,
        update: AuthSessionUpdate,
        at: UnixMillis,
    ) -> ServiceResult<AuthSessionRecord> {
        let current = self.get(tenant, id).await?;
        current.state.transition_to(next)?;
        let update = AuthSessionUpdate {
            state: Some(next),
            ..update
        };
        if !self
            .store
            .update_auth_session(tenant, id, &update, at)
            .await?
        {
            return Err(ServiceError::not_found("auth session", id.as_str()));
        }
        self.get(tenant, id).await
    }
}
