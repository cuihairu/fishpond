//! F034: the remote manual takeover service (T10).
//!
//! Binds short-lived operator credentials to the F011 coordination
//! state machine. Issuing a takeover flips the session AUTHORIZED →
//! TAKEN_OVER (automation stops) and mints exactly one operator
//! token — a short-lived, high-entropy credential whose raw value
//! exists only in the issue response. Verification is pure possession
//! (the auth-time path, no tenant key involved). Release revokes the
//! grant and hands the session back to automation (AUTHORIZED).

use std::sync::Arc;

use fishpond_domain::{AccountId, AuthSessionId, TenantId, UnixMillis};
use fishpond_storage::{NewTakeoverGrant, Store, generate_takeover_token, hash_takeover_token};

use crate::coordination::SessionCoordinator;
use crate::error::{ServiceError, ServiceResult};

/// The longest life an operator credential may carry. A takeover is a
/// human at the wheel, not a service account — credentials minutes
/// old are already stale.
pub const MAX_TTL_MS: i64 = 900_000;

/// What a verified token proves: who is driving which session of
/// which tenant, until when.
#[derive(Clone, Debug)]
pub struct TakeoverContext {
    /// The grant the token resolves to.
    pub grant_id: String,
    /// Owning tenant.
    pub tenant: TenantId,
    /// The account being driven.
    pub account: AccountId,
    /// The authorization session being driven.
    pub session: AuthSessionId,
    /// Who the credential was minted for.
    pub operator: String,
    /// The instant verification dies even without revocation.
    pub expires_at: UnixMillis,
}

/// Issues, verifies, and releases operator takeover credentials.
/// Clones share one coordinator — the per-account serialization gates
/// stay process-wide by construction.
#[derive(Clone)]
pub struct TakeoverService {
    store: Store,
    coordinator: Arc<SessionCoordinator>,
    max_ttl_ms: i64,
}

impl TakeoverService {
    /// Builds the service on top of the store and the session
    /// coordinator it drives state transitions through.
    pub fn new(store: Store, coordinator: SessionCoordinator) -> Self {
        Self {
            store,
            coordinator: Arc::new(coordinator),
            max_ttl_ms: MAX_TTL_MS,
        }
    }

    /// The configured credential lifetime cap.
    pub fn max_ttl_ms(&self) -> i64 {
        self.max_ttl_ms
    }

    /// Takes the session over (`AUTHORIZED → TAKEN_OVER`, automation
    /// stops) and mints the operator's credential. The raw token is
    /// returned exactly once; the database keeps only its hash. A
    /// session with a live grant refuses until it is released; an
    /// expired leftover grant is revoked to free the slot.
    pub async fn issue(
        &self,
        tenant: &TenantId,
        session_id: &AuthSessionId,
        operator: &str,
        ttl_ms: i64,
        at: UnixMillis,
    ) -> ServiceResult<(String, String, UnixMillis)> {
        if operator.trim().is_empty() {
            return Err(ServiceError::Invalid {
                what: "operator",
                reason: "must not be blank".to_owned(),
            });
        }
        if ttl_ms <= 0 || ttl_ms > self.max_ttl_ms {
            return Err(ServiceError::Invalid {
                what: "takeover lifetime",
                reason: format!("must be within 1..={} ms", self.max_ttl_ms),
            });
        }

        // One live credential per session. An expired leftover is
        // housekept (revoked) so the schema's live-uniqueness frees
        // the slot instead of blocking forever.
        for grant in self
            .store
            .list_takeover_grants(tenant, session_id.as_str())
            .await
            .map_err(ServiceError::Storage)?
            .iter()
            .filter(|grant| grant.revoked_at.is_none())
        {
            if grant.expires_at > at {
                return Err(ServiceError::Conflict {
                    what: "takeover grant",
                    id: session_id.as_str().to_owned(),
                    reason: "a live operator credential already exists; release it first"
                        .to_owned(),
                });
            }
            self.store
                .revoke_takeover_grant(tenant, &grant.id, at)
                .await
                .map_err(ServiceError::Storage)?;
        }

        let taken = self
            .coordinator
            .take_over(
                tenant,
                session_id,
                Some(format!("manual takeover: operator {operator}")),
                at,
            )
            .await?;

        let token = generate_takeover_token();
        let grant_id = format!("tg-{}", &hash_takeover_token(&token)[..16]);
        let expires_at = UnixMillis::from_millis(at.as_millis() + ttl_ms).map_err(|_| {
            ServiceError::Invalid {
                what: "takeover lifetime",
                reason: "expiry instant overflowed".to_owned(),
            }
        })?;
        self.store
            .issue_takeover_grant(&NewTakeoverGrant {
                id: &grant_id,
                tenant,
                account: taken.account_id.as_str(),
                session: session_id.as_str(),
                operator,
                token: &token,
                created_at: at,
                expires_at,
            })
            .await
            .map_err(ServiceError::Storage)?;
        Ok((grant_id, token, expires_at))
    }

    /// Verifies a raw token by possession alone — the operator
    /// console presents the credential, no tenant key involved. An
    /// unknown, expired, or revoked token is a NotFound that never
    /// echoes the presented material.
    pub async fn verify(&self, token: &str, now: UnixMillis) -> ServiceResult<TakeoverContext> {
        let grant = self
            .store
            .verify_takeover_token(token, now)
            .await
            .map_err(ServiceError::Storage)?
            .ok_or_else(|| ServiceError::not_found("takeover grant", "<token withheld>"))?;
        Ok(TakeoverContext {
            grant_id: grant.id,
            tenant: TenantId::parse(&grant.tenant_id).map_err(|_| ServiceError::Invalid {
                what: "takeover grant",
                reason: "stored tenant id failed to parse".to_owned(),
            })?,
            account: AccountId::parse(&grant.account_id).map_err(|_| ServiceError::Invalid {
                what: "takeover grant",
                reason: "stored account id failed to parse".to_owned(),
            })?,
            session: AuthSessionId::parse(&grant.session_id).map_err(|_| {
                ServiceError::Invalid {
                    what: "takeover grant",
                    reason: "stored session id failed to parse".to_owned(),
                }
            })?,
            operator: grant.operator,
            expires_at: grant.expires_at,
        })
    }

    /// Releases a takeover: revokes `grant_id` and hands the session
    /// back to automation. Reports whether a grant was actually
    /// released — a foreign tenant, a stale id, or an already-released
    /// grant is a no-op that leaves the takeover untouched.
    pub async fn release(
        &self,
        tenant: &TenantId,
        session_id: &AuthSessionId,
        grant_id: &str,
        at: UnixMillis,
    ) -> ServiceResult<bool> {
        let grants = self
            .store
            .list_takeover_grants(tenant, session_id.as_str())
            .await
            .map_err(ServiceError::Storage)?;
        // Only the session's own unrevoked grant releases it (expired
        // leftovers included — release is also the cleanup path).
        let Some(grant) = grants
            .iter()
            .find(|grant| grant.id == grant_id && grant.revoked_at.is_none())
        else {
            return Ok(false);
        };
        let revoked = self
            .store
            .revoke_takeover_grant(tenant, &grant.id, at)
            .await
            .map_err(ServiceError::Storage)?;
        if !revoked {
            return Ok(false);
        }
        self.coordinator.resume(tenant, session_id, at).await?;
        Ok(true)
    }
}
