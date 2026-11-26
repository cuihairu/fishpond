//! Authorization-session routes (F025): the REST mapping of
//! [`Sessions`]. Reads need `sessions:read`, transitions need
//! `sessions:write`. The list is cursor-paginated over the service's
//! deterministic `(created_at, id)` ordering; every failure is a
//! Problem document.

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

use fishpond_domain::AuthSessionId;

use super::AppState;
use super::accounts::require;
use super::auth::Caller;
use super::cursor;
use super::problem::Problem;
use fishpond_storage::AuthSessionRecord;

/// The session wire DTO (no credential material — only ids and pins).
#[derive(Debug, Serialize)]
pub struct SessionDto {
    /// Session id.
    pub id: String,
    /// The account being logged in.
    pub account_id: String,
    /// Platform (informational).
    pub platform: String,
    /// State-machine state (canonical name).
    pub state: String,
    /// Human qualifier, when present.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub state_note: Option<String>,
    /// The bound credential, once authorized.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub credential_id: Option<String>,
    /// Creation instant (unix ms).
    pub created_at: i64,
    /// Last state change (unix ms).
    pub updated_at: i64,
    /// When the session stops being usable (unix ms), if bounded.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<i64>,
}

impl From<&AuthSessionRecord> for SessionDto {
    fn from(record: &AuthSessionRecord) -> Self {
        SessionDto {
            id: record.id.as_str().to_owned(),
            account_id: record.account_id.as_str().to_owned(),
            platform: record.platform.clone(),
            state: record.state.as_str().to_owned(),
            state_note: record.state_note.clone(),
            credential_id: record.credential_id.clone(),
            created_at: record.created_at.as_millis(),
            updated_at: record.updated_at.as_millis(),
            expires_at: record.expires_at.map(|at| at.as_millis()),
        }
    }
}

impl From<AuthSessionRecord> for SessionDto {
    fn from(record: AuthSessionRecord) -> Self {
        SessionDto::from(&record)
    }
}

/// `GET /sessions` query parameters.
#[derive(Debug, Default, Deserialize)]
pub struct ListSessions {
    /// Opaque continuation cursor.
    pub cursor: Option<String>,
    /// Page size (default 50, cap 200).
    pub limit: Option<String>,
}

/// `POST /sessions` request body.
#[derive(Debug, Deserialize)]
pub struct StartSession {
    /// The session's id (client-chosen; unique per tenant).
    pub id: String,
    /// The account being logged in.
    pub account_id: String,
    /// Optional expiry (unix ms).
    pub expires_at: Option<i64>,
}

/// `POST /sessions/{id}/transition` request body: one named state
/// step, with the parameters that step takes.
#[derive(Debug, Deserialize)]
pub struct TransitionSession {
    /// `confirm` | `authorize` | `expire` | `revoke` | `fail` |
    /// `take-over` | `resume`.
    pub action: String,
    /// Human qualifier (confirm/revoke/fail/take-over).
    #[serde(default)]
    pub note: Option<String>,
    /// Bound credential (authorize).
    #[serde(default)]
    pub credential_id: Option<String>,
    /// The envelope generation the credential opened with (authorize).
    #[serde(default)]
    pub credential_version: Option<i32>,
    /// New expiry (authorize).
    #[serde(default)]
    pub expires_at: Option<i64>,
}

pub(super) fn parse_session_id(raw: &str) -> Result<AuthSessionId, Problem> {
    AuthSessionId::parse(raw.to_owned()).map_err(|err| {
        Problem::new(
            "invalid",
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid request",
            Some(format!("auth session id: {err}")),
        )
    })
}

fn parse_unix(raw: i64, what: &str) -> Result<fishpond_domain::UnixMillis, Problem> {
    fishpond_domain::UnixMillis::from_millis(raw).map_err(|err| {
        Problem::new(
            "invalid",
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid request",
            Some(format!("{what}: {err}")),
        )
    })
}

/// `GET /sessions` — one cursor page of the tenant's sessions.
pub async fn list(
    State(state): State<AppState>,
    caller: Caller,
    Query(params): Query<BTreeMap<String, String>>,
) -> Result<Json<serde_json::Value>, Problem> {
    require(&caller, "sessions:read")?;
    let limit = cursor::parse_limit(params.get("limit").map(String::as_str))?;
    let offset = match params.get("cursor").map(String::as_str) {
        Some(raw) => cursor::decode(raw)?,
        None => 0,
    };
    let sessions = state
        .sessions
        .list(&caller.tenant, None)
        .await
        .map_err(|err| Problem::from(&err))?;
    let page = cursor::paginate(sessions, offset, limit);
    let sessions: Vec<SessionDto> = page.items.iter().map(SessionDto::from).collect();
    Ok(Json(serde_json::json!({
        "sessions": sessions,
        "next_cursor": page.next_cursor,
    })))
}

/// `POST /sessions` — starts a login session (201 with the DTO).
pub async fn start(
    State(state): State<AppState>,
    caller: Caller,
    Json(body): Json<StartSession>,
) -> Result<(StatusCode, Json<SessionDto>), Problem> {
    require(&caller, "sessions:write")?;
    let id = parse_session_id(&body.id)?;
    let account_id = fishpond_domain::AccountId::parse(body.account_id.clone()).map_err(|err| {
        Problem::new(
            "invalid",
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid request",
            Some(format!("account id: {err}")),
        )
    })?;
    let expires_at = match body.expires_at {
        Some(raw) => Some(parse_unix(raw, "expires_at")?),
        None => None,
    };
    let record = state
        .sessions
        .start(&caller.tenant, id, account_id, super::now(), expires_at)
        .await
        .map_err(|err| Problem::from(&err))?;
    Ok((StatusCode::CREATED, Json(SessionDto::from(record))))
}

/// `GET /sessions/{id}` — one session (404 when absent or foreign).
pub async fn get(
    State(state): State<AppState>,
    caller: Caller,
    Path(id): Path<String>,
) -> Result<Json<SessionDto>, Problem> {
    require(&caller, "sessions:read")?;
    let session_id = parse_session_id(&id)?;
    let record = state
        .sessions
        .get(&caller.tenant, &session_id)
        .await
        .map_err(|err| Problem::from(&err))?;
    Ok(Json(SessionDto::from(record)))
}

/// `POST /sessions/{id}/transition` — one named state-machine step.
pub async fn transition(
    State(state): State<AppState>,
    caller: Caller,
    Path(id): Path<String>,
    Json(body): Json<TransitionSession>,
) -> Result<Json<SessionDto>, Problem> {
    require(&caller, "sessions:write")?;
    let session_id = parse_session_id(&id)?;
    let now = super::now();
    let record = match body.action.as_str() {
        "confirm" => {
            state
                .sessions
                .mark_confirmation(&caller.tenant, &session_id, body.note, now)
                .await
        }
        "authorize" => {
            let credential_id = body.credential_id.clone().ok_or_else(|| {
                Problem::new(
                    "invalid",
                    StatusCode::UNPROCESSABLE_ENTITY,
                    "invalid request",
                    Some("authorize: credential_id is required".to_owned()),
                )
            })?;
            let credential_version = body.credential_version.ok_or_else(|| {
                Problem::new(
                    "invalid",
                    StatusCode::UNPROCESSABLE_ENTITY,
                    "invalid request",
                    Some("authorize: credential_version is required".to_owned()),
                )
            })?;
            let expires_at = match body.expires_at {
                Some(raw) => Some(parse_unix(raw, "expires_at")?),
                None => None,
            };
            state
                .sessions
                .authorize(
                    &caller.tenant,
                    &session_id,
                    &credential_id,
                    credential_version,
                    expires_at,
                    now,
                )
                .await
        }
        "expire" => {
            state
                .sessions
                .expire(&caller.tenant, &session_id, now)
                .await
        }
        "revoke" => {
            state
                .sessions
                .revoke(&caller.tenant, &session_id, body.note, now)
                .await
        }
        "fail" => {
            state
                .sessions
                .fail(&caller.tenant, &session_id, body.note, now)
                .await
        }
        "take-over" => {
            state
                .sessions
                .take_over(&caller.tenant, &session_id, body.note, now)
                .await
        }
        "resume" => {
            state
                .sessions
                .resume(&caller.tenant, &session_id, now)
                .await
        }
        other => {
            return Err(Problem::new(
                "invalid",
                StatusCode::UNPROCESSABLE_ENTITY,
                "invalid request",
                Some(format!("unknown transition action {other}")),
            ));
        }
    };
    let record = record.map_err(|err| Problem::from(&err))?;
    Ok(Json(SessionDto::from(record)))
}
