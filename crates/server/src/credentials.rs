//! Credential lifecycle (F040): revocation with teeth.
//!
//! The REST face only ever revokes — issuing and rotating credentials
//! is the profile/worker face (F003/F022), which seals secrets with the
//! master-key envelope. Revoking here is deliberately coarse: the
//! credential is closed and every `AUTHORIZED` session carrying it is
//! revoked in the same transaction (the domain machine has no re-auth
//! state, so a dead carrier cannot keep sessions alive). Fresh logins
//! are already refused: [`Sessions::authorize`] loads the credential
//! and a revoked one refuses to open.

use crate::error::ServiceResult;
use fishpond_domain::{TenantId, UnixMillis};
use fishpond_storage::CredentialRevocation;

/// Shared, cloneable handle onto the persistence layer.
#[derive(Clone)]
pub struct Credentials {
    store: fishpond_storage::Store,
}

impl Credentials {
    /// Wraps an opened [`Store`].
    pub fn new(store: fishpond_storage::Store) -> Self {
        Self { store }
    }

    /// Revokes the credential and its carrying sessions atomically.
    /// Idempotent (a replay reports the original instant, zero session
    /// changes); a missing or foreign id is
    /// [`crate::error::ServiceError::NotFound`].
    pub async fn revoke(
        &self,
        tenant: &TenantId,
        id: &str,
        at: UnixMillis,
    ) -> ServiceResult<CredentialRevocation> {
        self.store
            .revoke_credential_with_sessions(tenant, id, at)
            .await
            .map_err(Into::into)
    }
}
