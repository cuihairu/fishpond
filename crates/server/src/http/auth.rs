//! REST authentication (F025): API keys bound to a tenant and a scope
//! set, presented as `Authorization: Bearer <key>`. Same model as the
//! MCP face (F009M) but server-local — the MCP crate sits above this
//! one, so the wire types cannot be shared upward.
//!
//! This is still the M0 in-memory seed store; F007's persisted key
//! store replaces it behind the same extractor without changing
//! handler shapes.

use axum::extract::FromRequestParts;
use axum::http::StatusCode;
use axum::http::request::Parts;
use fishpond_domain::TenantId;

use super::problem::Problem;

/// Tenant and scopes bound to one API key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestKeyRecord {
    /// Tenant the key belongs to (all data access is scoped to it).
    pub tenant_id: TenantId,
    /// Scopes granted, e.g. `accounts:read`.
    pub scopes: Vec<String>,
}

/// In-memory registry of REST API keys (M0 stand-in for F007).
#[derive(Debug, Default, Clone)]
pub struct RestKeys {
    records: std::collections::BTreeMap<String, RestKeyRecord>,
}

impl RestKeys {
    /// Creates an empty store.
    pub fn new() -> Self {
        Self::default()
    }

    /// Binds `key` to a tenant and scopes.
    pub fn insert(&mut self, key: impl Into<String>, record: RestKeyRecord) {
        self.records.insert(key.into(), record);
    }

    /// Resolves a presented key, if bound.
    pub fn authenticate(&self, key: &str) -> Option<RestKeyRecord> {
        self.records.get(key).cloned()
    }
}

/// The resolved caller of one REST request.
#[derive(Debug, Clone)]
pub struct Caller {
    /// Tenant whose data this call may touch.
    pub tenant: TenantId,
    /// Scopes granted to the presented key.
    pub scopes: Vec<String>,
}

impl Caller {
    /// Whether the presented key grants `scope`.
    pub fn has(&self, scope: &str) -> bool {
        self.scopes.iter().any(|granted| granted == scope)
    }
}

/// Extracts and authenticates the caller against the router state's
/// key store, producing RFC 9457 problems on failure: 401 for a
/// missing/unknown key, nothing else here (scope checks stay in
/// handlers, which know the required scope).
impl FromRequestParts<super::AppState> for Caller {
    type Rejection = Problem;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &super::AppState,
    ) -> Result<Self, Self::Rejection> {
        let header = parts
            .headers
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .ok_or_else(|| {
                Problem::new(
                    "unauthorized",
                    StatusCode::UNAUTHORIZED,
                    "unauthorized",
                    Some("missing Authorization header".to_owned()),
                )
            })?;
        let key = header
            .strip_prefix("Bearer ")
            .filter(|rest| !rest.is_empty())
            .ok_or_else(|| {
                Problem::new(
                    "unauthorized",
                    StatusCode::UNAUTHORIZED,
                    "unauthorized",
                    Some("Authorization must be `Bearer <key>`".to_owned()),
                )
            })?;
        state
            .keys
            .authenticate(key)
            .map(|record| Caller {
                tenant: record.tenant_id,
                scopes: record.scopes,
            })
            .ok_or_else(|| {
                Problem::new(
                    "unauthorized",
                    StatusCode::UNAUTHORIZED,
                    "unauthorized",
                    Some("unknown api key".to_owned()),
                )
            })
    }
}
