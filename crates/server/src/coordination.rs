//! Session coordination (F011): one live session per account, credential
//! version pinning and manual takeover — the automation gatekeeper.
//!
//! The coordinator sits on top of [`crate::sessions::Sessions`] (state
//! machine) and an [`AccountSupervisor`] (per-account actors, F010) and
//! owns three cross-session rules:
//!
//! * **Mutual exclusion** — at most one *live* session per account
//!   (`AWAITING_CONFIRMATION`, `AUTHORIZED` or `TAKEN_OVER`). Per-account
//!   gates serialize check-then-act, so two concurrent authorizes cannot
//!   both win. `PENDING` is deliberately not exclusive: a stale login
//!   attempt must never brick the account's next login.
//! * **Credential versioning** — a session pins the envelope generation
//!   it was authorized with. If the credential rotates underneath, the
//!   session is expired and automation must re-authenticate.
//! * **Manual takeover** — an operator flips `AUTHORIZED → TAKEN_OVER`;
//!   the account's actor is stopped and automation is refused until the
//!   session is resumed (and the actor re-attached).

use fishpond_domain::{AccountId, AuthSessionId, AuthSessionState, TenantId, UnixMillis};
use fishpond_runtime::{AccountSupervisor, ActorBehavior, MailboxSender, RestartPolicy};
use fishpond_storage::AuthSessionRecord;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::error::{ServiceError, ServiceResult};
use crate::sessions::Sessions;

/// States that block another session on the same account.
fn is_exclusive(state: AuthSessionState) -> bool {
    matches!(
        state,
        AuthSessionState::AwaitingConfirmation
            | AuthSessionState::Authorized
            | AuthSessionState::TakenOver
    )
}

/// The session coordinator (see module docs).
pub struct SessionCoordinator {
    sessions: Sessions,
    supervisor: Arc<tokio::sync::Mutex<AccountSupervisor>>,
    /// One gate per account, created on demand; the lock map itself is
    /// coarse-grained on purpose (coordination operations are rare).
    gates: Mutex<HashMap<AccountId, Arc<tokio::sync::Mutex<()>>>>,
}

impl SessionCoordinator {
    /// Builds a coordinator over the given session service.
    pub fn new(sessions: Sessions) -> Self {
        Self {
            sessions,
            supervisor: Arc::new(tokio::sync::Mutex::new(AccountSupervisor::new())),
            gates: Mutex::new(HashMap::new()),
        }
    }

    /// The per-account gate, created on demand. Callers bind the
    /// returned handle and lock it for the duration of their
    /// check-then-act window (binding first keeps the gate alive while
    /// the guard holds it).
    fn gate_for(&self, account: &AccountId) -> Arc<tokio::sync::Mutex<()>> {
        self.gates
            .lock()
            .expect("gate map lock")
            .entry(account.clone())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone()
    }

    /// Authorizes `session_id` for its account, refusing when another
    /// live session already holds the account (mutual exclusion). The
    /// check and the transition happen inside the per-account gate.
    pub async fn authorize_exclusive(
        &self,
        tenant: &TenantId,
        session_id: &AuthSessionId,
        credential_id: &str,
        credential_version: i32,
        expires_at: Option<UnixMillis>,
        at: UnixMillis,
    ) -> ServiceResult<AuthSessionRecord> {
        let session = self.sessions.get(tenant, session_id).await?;
        let gate = self.gate_for(&session.account_id);
        let _guard = gate.lock().await;
        if let Some(live) = self
            .first_live_other_than(tenant, &session.account_id, session_id)
            .await?
        {
            return Err(ServiceError::conflict(
                "auth session",
                live.id.as_str(),
                format!(
                    "account {} already has a live session ({})",
                    session.account_id, live.id
                ),
            ));
        }
        self.sessions
            .authorize(
                tenant,
                session_id,
                credential_id,
                credential_version,
                expires_at,
                at,
            )
            .await
    }

    /// The account's live session that is not `session_id`, if any.
    async fn first_live_other_than(
        &self,
        tenant: &TenantId,
        account: &AccountId,
        session_id: &AuthSessionId,
    ) -> ServiceResult<Option<AuthSessionRecord>> {
        let sessions = self.sessions.list(tenant, Some(account)).await?;
        Ok(sessions
            .into_iter()
            .find(|other| other.id != *session_id && is_exclusive(other.state)))
    }

    /// Attaches an automation actor for `account`, but only while a live
    /// `AUTHORIZED` session backs it (see [`Self::ensure_automatable`]).
    /// Returns the actor's mailbox sender.
    pub async fn attach<B>(
        &self,
        tenant: &TenantId,
        account: AccountId,
        behavior: B,
        capacity: usize,
    ) -> ServiceResult<MailboxSender<B::Message>>
    where
        B: ActorBehavior,
    {
        let gate = self.gate_for(&account);
        let _guard = gate.lock().await;
        self.ensure_automatable(tenant, &account).await?;
        let mut supervisor = self.supervisor.lock().await;
        Ok(supervisor.start(account, behavior, capacity, RestartPolicy::standard()))
    }

    /// Whether the coordinator currently runs an actor for `account`.
    pub async fn is_attached(&self, account: &AccountId) -> bool {
        let supervisor = self.supervisor.lock().await;
        supervisor.contains(account)
    }

    /// Manual takeover: `AUTHORIZED → TAKEN_OVER`, stopping the
    /// account's actor. A second takeover on an already taken-over
    /// session is an illegal transition and errors (the machine, not the
    /// coordinator, decides).
    pub async fn take_over(
        &self,
        tenant: &TenantId,
        session_id: &AuthSessionId,
        note: Option<String>,
        at: UnixMillis,
    ) -> ServiceResult<AuthSessionRecord> {
        let session = self.sessions.get(tenant, session_id).await?;
        let gate = self.gate_for(&session.account_id);
        let _guard = gate.lock().await;
        let taken = self
            .sessions
            .take_over(tenant, session_id, note, at)
            .await?;
        let mut supervisor = self.supervisor.lock().await;
        supervisor.remove(&session.account_id);
        Ok(taken)
    }

    /// Resumes automation after a takeover: `TAKEN_OVER → AUTHORIZED`.
    /// The actor is *not* resurrected (its state died with the stop);
    /// callers re-attach via [`Self::attach`].
    pub async fn resume(
        &self,
        tenant: &TenantId,
        session_id: &AuthSessionId,
        at: UnixMillis,
    ) -> ServiceResult<AuthSessionRecord> {
        let session = self.sessions.get(tenant, session_id).await?;
        let gate = self.gate_for(&session.account_id);
        let _guard = gate.lock().await;
        self.sessions.resume(tenant, session_id, at).await
    }

    /// The automation preflight: the account's live session must exist,
    /// be `AUTHORIZED`, and still be pinned to the *current* generation
    /// of its credential. A rotated credential expires the session and
    /// reports [`ServiceError::ReauthRequired`]; a taken-over session
    /// reports a conflict.
    pub async fn ensure_automatable(
        &self,
        tenant: &TenantId,
        account: &AccountId,
    ) -> ServiceResult<AuthSessionRecord> {
        let live = self
            .sessions
            .list(tenant, Some(account))
            .await?
            .into_iter()
            .find(|session| is_exclusive(session.state))
            .ok_or_else(|| {
                ServiceError::conflict("auth session", account.as_str(), "no live session")
            })?;
        if live.state == AuthSessionState::TakenOver {
            return Err(ServiceError::conflict(
                "auth session",
                live.id.as_str(),
                "session is under manual takeover",
            ));
        }
        if live.state != AuthSessionState::Authorized {
            return Err(ServiceError::conflict(
                "auth session",
                live.id.as_str(),
                format!("session is {} and not usable", live.state),
            ));
        }
        self.ensure_credential_current(tenant, live).await
    }

    /// Validates the session's credential pin against the stored
    /// envelope's generation; on rotation the session is expired and
    /// [`ServiceError::ReauthRequired`] is returned.
    pub async fn ensure_credential_current(
        &self,
        tenant: &TenantId,
        session: AuthSessionRecord,
    ) -> ServiceResult<AuthSessionRecord> {
        let Some(credential_id) = session.credential_id.as_deref() else {
            return Err(ServiceError::conflict(
                "auth session",
                session.id.as_str(),
                "session carries no credential",
            ));
        };
        let current = self
            .sessions
            .credential_version(tenant, credential_id)
            .await?;
        let Some(pinned) = session.credential_version else {
            return Err(ServiceError::conflict(
                "auth session",
                session.id.as_str(),
                "session pins no credential version",
            ));
        };
        if pinned != current {
            self.sessions
                .expire(tenant, &session.id, session.updated_at)
                .await?;
            return Err(ServiceError::ReauthRequired {
                session: session.id.as_str().to_owned(),
                pinned,
                current,
            });
        }
        Ok(session)
    }

    /// Expires every authorized session of `tenant` whose TTL ran out
    /// (idempotent sweep helper for the scheduler).
    pub async fn expire_stale(&self, tenant: &TenantId, now: UnixMillis) -> ServiceResult<usize> {
        let mut expired = 0;
        for session in self.sessions.list(tenant, None).await? {
            let overdue = matches!(session.state, AuthSessionState::Authorized)
                && session.expires_at.is_some_and(|until| until <= now);
            if overdue && self.sessions.expire(tenant, &session.id, now).await.is_ok() {
                expired += 1;
            }
        }
        Ok(expired)
    }

    /// Loads the session for read-only callers (exposed so the wire
    /// layer does not need the raw [`Sessions`] handle).
    pub async fn get(
        &self,
        tenant: &TenantId,
        session_id: &AuthSessionId,
    ) -> ServiceResult<AuthSessionRecord> {
        self.sessions.get(tenant, session_id).await
    }
}
