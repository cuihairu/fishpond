//! Problem Details (RFC 9457) mapping (F025): every REST failure is a
//! `application/problem+json` document with a stable `type` URI, so
//! clients branch on machine-readable types instead of parsing prose.
//!
//! Details carry resource kinds, ids and reasons — never credentials
//! or page content. Internal storage failures stay generic: the
//! response says the store refused, nothing about the SQL beneath.

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use fishpond_storage::StorageError;
use serde::Serialize;

use crate::error::ServiceError;

/// The base URI every problem `type` resolves against.
pub const PROBLEM_BASE: &str = "https://fishpond.dev/problems/";

/// One RFC 9457 problem document.
#[derive(Debug, Serialize)]
pub struct Problem {
    /// A URI identifying the failure class.
    #[serde(rename = "type")]
    pub type_uri: String,
    /// Short human summary of the class.
    pub title: &'static str,
    /// The HTTP status (repeated in the body for log-less clients).
    pub status: u16,
    /// What happened for this occurrence (no secrets).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

impl Problem {
    /// Builds a problem for a class name and status.
    pub fn new(
        class: &str,
        status: StatusCode,
        title: &'static str,
        detail: Option<String>,
    ) -> Self {
        Problem {
            type_uri: format!("{PROBLEM_BASE}{class}"),
            title,
            status: status.as_u16(),
            detail,
        }
    }

    /// The status this problem maps to.
    pub fn status(&self) -> StatusCode {
        StatusCode::from_u16(self.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR)
    }
}

impl IntoResponse for Problem {
    fn into_response(self) -> Response {
        let status = self.status();
        (
            status,
            [("content-type", "application/problem+json")],
            Json(self),
        )
            .into_response()
    }
}

impl From<&ServiceError> for Problem {
    fn from(err: &ServiceError) -> Self {
        match err {
            ServiceError::NotFound { what, id } => Problem::new(
                "not-found",
                StatusCode::NOT_FOUND,
                "not found",
                Some(format!("{what} {id} not found")),
            ),
            ServiceError::Invalid { what, reason } => Problem::new(
                "invalid",
                StatusCode::UNPROCESSABLE_ENTITY,
                "invalid request",
                Some(format!("{what}: {reason}")),
            ),
            ServiceError::Conflict { what, id, reason } => Problem::new(
                "conflict",
                StatusCode::CONFLICT,
                "conflict",
                Some(format!("{what} {id}: {reason}")),
            ),
            ServiceError::ReauthRequired { session, .. } => Problem::new(
                "reauth-required",
                StatusCode::UNAUTHORIZED,
                "re-authorization required",
                Some(format!("session {session} must be re-authorized")),
            ),
            // Persistence failures stay generic: the client learns the
            // store refused, never which query or why.
            ServiceError::Storage(_) => Problem::new(
                "storage",
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal storage failure",
                None,
            ),
            // Domain errors share the mapping: a state-machine
            // refusal is a conflict, the rest is invalid input.
            ServiceError::Domain(err) => Problem::from(err),
        }
    }
}

impl From<ServiceError> for Problem {
    fn from(err: ServiceError) -> Self {
        Problem::from(&err)
    }
}

impl From<&fishpond_domain::DomainError> for Problem {
    fn from(err: &fishpond_domain::DomainError) -> Self {
        match err {
            // A state-machine refusal conflicts with the resource's
            // current state (RFC 9457), not with the request syntax.
            fishpond_domain::DomainError::InvalidTransition { machine, from, to } => Problem::new(
                "conflict",
                StatusCode::CONFLICT,
                "conflict",
                Some(format!(
                    "illegal {machine} state transition: {from} -> {to}"
                )),
            ),
            _ => Problem::new(
                "invalid",
                StatusCode::UNPROCESSABLE_ENTITY,
                "invalid request",
                Some(err.to_string()),
            ),
        }
    }
}

impl From<&StorageError> for Problem {
    fn from(err: &StorageError) -> Self {
        match err {
            StorageError::NotFound { what, id } => Problem::new(
                "not-found",
                StatusCode::NOT_FOUND,
                "not found",
                Some(format!("{what} {id} not found")),
            ),
            StorageError::Invalid { what, reason } => Problem::new(
                "invalid",
                StatusCode::UNPROCESSABLE_ENTITY,
                "invalid request",
                Some(format!("{what}: {reason}")),
            ),
            StorageError::Conflict { what, id, reason } => Problem::new(
                "conflict",
                StatusCode::CONFLICT,
                "conflict",
                Some(format!("{what} {id}: {reason}")),
            ),
            _ => Problem::new(
                "storage",
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal storage failure",
                None,
            ),
        }
    }
}
