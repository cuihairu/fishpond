//! API key + scope authorization shared by every MCP tool.
//!
//! The MCP face reuses the REST authorization model: a caller presents an
//! API key that resolves to a tenant and a set of scopes. Cross-tenant
//! access is impossible because every service call carries the
//! [`AuthContext`] of the presenting key.
//!
//! M0 note (F009M): keys come from an [`ApiKeyStore`] seeded by the CLI
//! bootstrap or by tests. F007/F025 replace this with the persistent
//! tenant/API-key store; the [`AuthContext`] shape is the stable seam.

use std::collections::BTreeMap;

/// Environment variable holding the API key for the stdio transport.
pub const ENV_API_KEY: &str = "FISHPOND_API_KEY";

/// A named permission required by a tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Scope(&'static str);

impl Scope {
    /// Read access to accounts.
    pub const ACCOUNTS_READ: Scope = Scope("accounts:read");
    /// Read access to conversations and messages.
    pub const CONVERSATIONS_READ: Scope = Scope("conversations:read");
    /// Send messages (write path, Mock provider only in M0).
    pub const CONVERSATIONS_WRITE: Scope = Scope("conversations:write");
    /// Read access to the product catalog.
    pub const CATALOG_READ: Scope = Scope("catalog:read");
    /// Read access to operations and attempts.
    pub const OPERATIONS_READ: Scope = Scope("operations:read");
    /// Read access to the event stream.
    pub const EVENTS_READ: Scope = Scope("events:read");

    /// Every scope known to the M0 MCP face.
    pub const ALL: [Scope; 6] = [
        Scope::ACCOUNTS_READ,
        Scope::CONVERSATIONS_READ,
        Scope::CONVERSATIONS_WRITE,
        Scope::CATALOG_READ,
        Scope::OPERATIONS_READ,
        Scope::EVENTS_READ,
    ];

    /// The scope name as used in configuration and error messages.
    pub fn as_str(&self) -> &'static str {
        self.0
    }
}

impl std::fmt::Display for Scope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}

/// How the caller attached to the current session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transport {
    /// Stdio transport: the key came from `FISHPOND_API_KEY`.
    Stdio,
    /// Streamable HTTP transport: the key came from request headers.
    Http,
}

/// Resolved identity of the caller of one tool invocation.
#[derive(Debug, Clone)]
pub struct AuthContext {
    /// Tenant whose data this call may touch.
    pub tenant_id: String,
    /// Scopes granted to the presented API key.
    pub scopes: Vec<Scope>,
    /// Transport the key was presented on.
    pub transport: Transport,
}

impl AuthContext {
    /// Whether the presented key grants `scope`.
    pub fn has(&self, scope: Scope) -> bool {
        self.scopes.contains(&scope)
    }
}

/// Tenant and scopes bound to one API key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApiKeyRecord {
    /// Tenant owning the key.
    pub tenant_id: String,
    /// Scopes granted by the key.
    pub scopes: Vec<Scope>,
}

/// In-memory registry of API keys (M0 stand-in for the F007 store).
#[derive(Debug, Default)]
pub struct ApiKeyStore {
    records: BTreeMap<String, ApiKeyRecord>,
}

impl ApiKeyStore {
    /// Creates an empty store.
    pub fn new() -> Self {
        Self::default()
    }

    /// Binds `key` to a tenant and scopes.
    pub fn insert(&mut self, key: String, record: ApiKeyRecord) {
        self.records.insert(key, record);
    }

    /// Resolves `key` to its record, or `None` for unknown keys.
    pub fn resolve(&self, key: &str) -> Option<&ApiKeyRecord> {
        self.records.get(key)
    }
}

/// Extracts an API key from streamable-HTTP request headers.
///
/// Accepted headers: `x-fishpond-api-key: <key>` or
/// `authorization: Bearer <key>`. This is a pure header lookup; it never
/// logs the key material.
pub fn extract_key_from_headers(headers: &axum::http::HeaderMap) -> Option<String> {
    if let Some(value) = headers.get("x-fishpond-api-key")
        && let Ok(text) = value.to_str()
    {
        return Some(text.trim().to_owned());
    }
    if let Some(value) = headers.get("authorization")
        && let Ok(text) = value.to_str()
    {
        let text = text.trim();
        if let Some(rest) = text
            .strip_prefix("Bearer ")
            .or_else(|| text.strip_prefix("bearer "))
        {
            return Some(rest.trim().to_owned());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(pairs: &[(&str, &str)]) -> axum::http::HeaderMap {
        let mut map = axum::http::HeaderMap::new();
        for (name, value) in pairs {
            map.insert(
                axum::http::HeaderName::from_bytes(name.as_bytes()).unwrap(),
                axum::http::HeaderValue::from_str(value).unwrap(),
            );
        }
        map
    }

    #[test]
    fn resolves_registered_key_to_record() {
        let mut store = ApiKeyStore::new();
        store.insert(
            "fp_k1".to_owned(),
            ApiKeyRecord {
                tenant_id: "t-a".to_owned(),
                scopes: vec![Scope::ACCOUNTS_READ],
            },
        );
        let record = store.resolve("fp_k1").expect("registered key resolves");
        assert_eq!(record.tenant_id, "t-a");
        assert!(store.resolve("fp_unknown").is_none());
    }

    #[test]
    fn extracts_key_from_both_header_styles() {
        let direct = headers(&[("x-fishpond-api-key", "fp_direct")]);
        assert_eq!(
            extract_key_from_headers(&direct),
            Some("fp_direct".to_owned())
        );

        let bearer = headers(&[("authorization", "Bearer fp_bearer")]);
        assert_eq!(
            extract_key_from_headers(&bearer),
            Some("fp_bearer".to_owned())
        );

        let none = headers(&[("content-type", "application/json")]);
        assert_eq!(extract_key_from_headers(&none), None);
    }

    #[test]
    fn auth_context_scope_check() {
        let ctx = AuthContext {
            tenant_id: "t-a".to_owned(),
            scopes: vec![Scope::ACCOUNTS_READ, Scope::CONVERSATIONS_READ],
            transport: Transport::Stdio,
        };
        assert!(ctx.has(Scope::ACCOUNTS_READ));
        assert!(!ctx.has(Scope::CONVERSATIONS_WRITE));
    }
}
