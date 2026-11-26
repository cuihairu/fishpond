//! Authorization precedence (F016): which API-key scope gates a
//! capability, and in which order the gates apply.
//!
//! The mapping is explicit and least-privilege: every routable
//! capability names the one frozen RBAC scope (F007) a caller must
//! hold. Capabilities with no frozen scope (`orders:manage`, and any
//! future upstream face) are *not routable* — a key's privileges never
//! conjure a missing scope, so unconfirmed upstream surfaces stay
//! unreachable (§red line: 未确认 Provider 不得启用).
//!
//! Precedence, evaluated in order:
//! 1. **RBAC scope** — the resolved key must hold the capability's
//!    scope (fail-closed; a denied key learns nothing further).
//! 2. **Routability** — the capability must map to a scope at all.
//! 3. **Provider support** — checked at route selection
//!    ([`crate::router`]), not here.
//!
//! A capability that maps to a scope but whose key lacks it fails with
//! [`RouteError::ScopeDenied`]; a capability that maps to nothing
//! fails with [`RouteError::NotRoutable`] *regardless of the key* —
//! even an all-scope key cannot route `orders:manage` today.

use fishpond_provider_sdk::Capability;
use fishpond_storage::ResolvedKey;

use crate::error::{RouteError, RouteResult};

/// The frozen scope a caller needs for `capability`, or `None` when no
/// scope gates it yet (the capability is not routable).
pub fn required_scope(capability: Capability) -> Option<&'static str> {
    match capability {
        Capability::AccountState => Some(fishpond_storage::SCOPE_ACCOUNTS_READ),
        Capability::Authorize => Some(fishpond_storage::SCOPE_ACCOUNTS_READ),
        Capability::ReadConversations => Some(fishpond_storage::SCOPE_CONVERSATIONS_READ),
        Capability::SendMessage => Some(fishpond_storage::SCOPE_CONVERSATIONS_WRITE),
        Capability::ReadCatalog => Some(fishpond_storage::SCOPE_CATALOG_READ),
        // No frozen scope gates order management; real order surfaces
        // arrive with an authorized provider (F032+) and their own
        // scope. Until then: not routable, for every key.
        Capability::ManageOrders => None,
        Capability::StreamEvents => Some(fishpond_storage::SCOPE_EVENTS_READ),
    }
}

/// Gate 1+2 of the precedence: the resolved key must hold the
/// capability's scope; a capability with no scope is not routable for
/// *any* key. Returns `Ok(())` when the call may proceed to provider
/// selection.
pub fn authorize_scope(key: &ResolvedKey, capability: Capability) -> RouteResult<()> {
    match required_scope(capability) {
        Some(scope) if key.has_scope(scope) => Ok(()),
        Some(scope) => Err(RouteError::ScopeDenied { scope }),
        None => Err(RouteError::NotRoutable { capability }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fishpond_domain::TenantId;

    fn key(scopes: &[&str]) -> ResolvedKey {
        ResolvedKey {
            tenant_id: TenantId::parse("t-scope-test").expect("tenant id"),
            role: "operator".to_owned(),
            scopes: scopes.iter().map(|s| (*s).to_owned()).collect(),
            expires_at: fishpond_domain::UnixMillis::from_millis(9_000).expect("non-negative"),
        }
    }

    #[test]
    fn scoped_capabilities_map_to_frozen_scopes() {
        assert_eq!(
            required_scope(Capability::AccountState),
            Some("accounts:read")
        );
        assert_eq!(
            required_scope(Capability::ReadConversations),
            Some("conversations:read")
        );
        assert_eq!(
            required_scope(Capability::SendMessage),
            Some("conversations:write")
        );
        assert_eq!(
            required_scope(Capability::ReadCatalog),
            Some("catalog:read")
        );
        assert_eq!(
            required_scope(Capability::StreamEvents),
            Some("events:read")
        );
    }

    #[test]
    fn unscoped_capabilities_map_to_nothing() {
        assert_eq!(required_scope(Capability::ManageOrders), None);
    }

    #[test]
    fn precedence_scope_wins_then_routability() {
        // Gate 1: a key holding the scope passes.
        assert!(authorize_scope(&key(&["conversations:write"]), Capability::SendMessage).is_ok());
        // Gate 1: a key without the scope is denied, naming the scope.
        let err =
            authorize_scope(&key(&["catalog:read"]), Capability::SendMessage).expect_err("denied");
        let RouteError::ScopeDenied { scope } = err else {
            panic!("expected ScopeDenied, got {err}");
        };
        assert_eq!(scope, "conversations:write");
        // Gate 2: not routable even for a key with every frozen scope.
        let err = authorize_scope(&key(&fishpond_storage::SCOPE_ALL), Capability::ManageOrders)
            .expect_err("unscoped capability");
        assert!(matches!(err, RouteError::NotRoutable { .. }), "got {err}");
    }
}
