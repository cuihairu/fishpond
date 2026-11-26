//! Takeover routes (F034): the REST mapping of [`TakeoverService`].
//! Issuing and releasing a takeover needs `sessions:takeover`; the
//! grant audit list needs `sessions:read`. Verification is the
//! operator console's possession path — it takes the takeover token
//! itself and no bearer key, exactly like the service face.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use serde::{Deserialize, Serialize};

use fishpond_storage::TakeoverGrantRecord;

use super::AppState;
use super::accounts::require;
use super::auth::Caller;
use super::problem::Problem;
use super::sessions::parse_session_id;
use crate::takeover::TakeoverContext;

/// `POST /sessions/{id}/takeover` request body.
#[derive(Debug, Deserialize)]
pub struct StartTakeover {
    /// Who the credential is for (audit display).
    pub operator: String,
    /// Requested life in ms; defaults to the service cap.
    #[serde(default)]
    pub ttl_ms: Option<i64>,
}

/// The issue response: the raw token appears exactly once, here, and
/// is never recoverable again.
#[derive(Debug, Serialize)]
pub struct GrantIssued {
    /// The grant this response minted.
    pub grant_id: String,
    /// The raw credential — store it now; the server only holds a hash.
    pub token: String,
    /// When verification dies even without release (unix ms).
    pub expires_at: i64,
}

/// The grant audit DTO: hashes and display tails only, never tokens.
#[derive(Debug, Serialize)]
pub struct GrantDto {
    /// Grant id.
    pub id: String,
    /// Who the credential was minted for.
    pub operator: String,
    /// The raw token's display tail.
    pub token_hint: String,
    /// Issue instant (unix ms).
    pub created_at: i64,
    /// The life's end (unix ms).
    pub expires_at: i64,
    /// When revoked, if it was (unix ms).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub revoked_at: Option<i64>,
}

impl From<&TakeoverGrantRecord> for GrantDto {
    fn from(record: &TakeoverGrantRecord) -> Self {
        GrantDto {
            id: record.id.clone(),
            operator: record.operator.clone(),
            token_hint: record.token_hint.clone(),
            created_at: record.created_at.as_millis(),
            expires_at: record.expires_at.as_millis(),
            revoked_at: record.revoked_at.map(|at| at.as_millis()),
        }
    }
}

/// The verified context: what the presented token proves.
#[derive(Debug, Serialize)]
pub struct ContextDto {
    /// The grant the token resolves to.
    pub grant_id: String,
    /// Owning tenant.
    pub tenant: String,
    /// The account being driven.
    pub account: String,
    /// The authorization session being driven.
    pub session: String,
    /// Who the credential was minted for.
    pub operator: String,
    /// When verification dies even without release (unix ms).
    pub expires_at: i64,
}

impl From<&TakeoverContext> for ContextDto {
    fn from(context: &TakeoverContext) -> Self {
        ContextDto {
            grant_id: context.grant_id.clone(),
            tenant: context.tenant.as_str().to_owned(),
            account: context.account.as_str().to_owned(),
            session: context.session.as_str().to_owned(),
            operator: context.operator.clone(),
            expires_at: context.expires_at.as_millis(),
        }
    }
}

/// `POST /sessions/{id}/takeover` — takes the session over and mints
/// the operator credential (201; the token is shown exactly once).
pub async fn issue(
    State(state): State<AppState>,
    caller: Caller,
    Path(id): Path<String>,
    Json(body): Json<StartTakeover>,
) -> Result<(StatusCode, Json<GrantIssued>), Problem> {
    require(&caller, "sessions:takeover")?;
    let session_id = parse_session_id(&id)?;
    let ttl_ms = body.ttl_ms.unwrap_or(crate::takeover::MAX_TTL_MS);
    let (grant_id, token, expires_at) = state
        .takeover
        .issue(
            &caller.tenant,
            &session_id,
            &body.operator,
            ttl_ms,
            super::now(),
        )
        .await
        .map_err(|err| Problem::from(&err))?;
    Ok((
        StatusCode::CREATED,
        Json(GrantIssued {
            grant_id,
            token,
            expires_at: expires_at.as_millis(),
        }),
    ))
}

/// `DELETE /sessions/{id}/takeover/{grant_id}` — releases the
/// takeover: the credential dies and automation resumes.
pub async fn release(
    State(state): State<AppState>,
    caller: Caller,
    Path((id, grant_id)): Path<(String, String)>,
) -> Result<Json<serde_json::Value>, Problem> {
    require(&caller, "sessions:takeover")?;
    let session_id = parse_session_id(&id)?;
    let released = state
        .takeover
        .release(&caller.tenant, &session_id, &grant_id, super::now())
        .await
        .map_err(|err| Problem::from(&err))?;
    if !released {
        return Err(Problem::from(&crate::error::ServiceError::not_found(
            "takeover grant",
            grant_id,
        )));
    }
    Ok(Json(serde_json::json!({ "released": true })))
}

/// `GET /sessions/{id}/takeover` — the session's grant audit list
/// (hashes and hints; never raw tokens).
pub async fn list(
    State(state): State<AppState>,
    caller: Caller,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, Problem> {
    require(&caller, "sessions:read")?;
    let session_id = parse_session_id(&id)?;
    let grants = state
        .store
        .list_takeover_grants(&caller.tenant, session_id.as_str())
        .await
        .map_err(|err| Problem::from(&crate::error::ServiceError::Storage(err)))?;
    let grants: Vec<GrantDto> = grants.iter().map(GrantDto::from).collect();
    Ok(Json(serde_json::json!({ "grants": grants })))
}

/// `POST /takeover/verify` — the operator console's possession path.
/// The takeover token itself is the credential (`X-Takeover-Token`);
/// no bearer key is involved. An unknown, expired, or revoked token is
/// a 404 that never echoes the presented material.
pub async fn verify(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<ContextDto>, Problem> {
    let token = headers
        .get("x-takeover-token")
        .and_then(|value| value.to_str().ok())
        .ok_or_else(|| {
            Problem::new(
                "unauthorized",
                StatusCode::UNAUTHORIZED,
                "unauthorized",
                Some("the X-Takeover-Token header carries the credential".to_owned()),
            )
        })?;
    let context = state
        .takeover
        .verify(token, super::now())
        .await
        .map_err(|err| Problem::from(&err))?;
    Ok(Json(ContextDto::from(&context)))
}
