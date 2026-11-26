//! Operation routes (F025): the REST face over the F014 reliable-task
//! store. `POST /operations` is the idempotent create — the client
//! picks the id and an `Idempotency-Key` header; replaying the same
//! key returns the original operation (200) instead of new work (201).
//! Claim and close mirror the storage CAS contracts: claim moves a
//! PENDING operation to RUNNING with a numbered attempt, close settles
//! the attempt (success/fail ends the operation, timeout returns it to
//! PENDING).

use std::collections::BTreeMap;

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use fishpond_domain::{AttemptId, Operation, OperationId, WorkerId};
use fishpond_storage::{AttemptClose, CreatedOperation};

use super::AppState;
use super::accounts::require;
use super::auth::Caller;
use super::cursor;
use super::problem::Problem;

/// `POST /operations` body.
#[derive(Debug, serde::Deserialize)]
pub struct CreateOperation {
    /// Client-chosen operation id.
    pub id: String,
    /// What to do (e.g. `send_message`).
    pub kind: String,
}

/// `POST /operations/{id}/claim` body.
#[derive(Debug, serde::Deserialize)]
pub struct ClaimOperation {
    /// The claiming worker.
    pub worker_id: String,
    /// Client-chosen attempt id.
    pub attempt_id: String,
    /// Attempt deadline (unix millis).
    pub deadline_at: i64,
}

/// `POST /operations/{id}/attempts/{attempt_id}/close` body.
#[derive(Debug, serde::Deserialize)]
pub struct CloseAttempt {
    /// The worker that ran the attempt.
    pub worker_id: String,
    /// `succeeded`, `failed` or `timed-out`.
    pub result: String,
}

fn operation_json(operation: &Operation) -> serde_json::Value {
    serde_json::json!({
        "id": operation.id.as_str(),
        "kind": operation.kind,
        "state": operation.state.as_str(),
        "created_at": operation.created_at.as_millis(),
    })
}

fn parse_operation_id(raw: &str) -> Result<OperationId, Problem> {
    OperationId::parse(raw.to_owned()).map_err(|err| Problem::from(&err))
}

fn parse_attempt_id(raw: &str) -> Result<AttemptId, Problem> {
    AttemptId::parse(raw.to_owned()).map_err(|err| Problem::from(&err))
}

fn parse_worker_id(raw: &str) -> Result<WorkerId, Problem> {
    WorkerId::parse(raw.to_owned()).map_err(|err| Problem::from(&err))
}

fn parse_deadline(raw: i64) -> Result<fishpond_domain::UnixMillis, Problem> {
    fishpond_domain::UnixMillis::from_millis(raw).map_err(|err| Problem::from(&err))
}

fn parse_close(raw: &str) -> Result<AttemptClose, Problem> {
    match raw {
        "succeeded" => Ok(AttemptClose::Succeeded),
        "failed" => Ok(AttemptClose::Failed),
        "timed-out" => Ok(AttemptClose::TimedOut),
        other => Err(Problem::new(
            "invalid",
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid request",
            Some(format!("result: unknown close result {other}")),
        )),
    }
}

/// `POST /operations` — idempotent create. The `Idempotency-Key`
/// header is mandatory (400 when missing); the same key with the same
/// id+kind replays the original (200), the same key with different
/// work is a 409.
pub async fn create(
    State(state): State<AppState>,
    caller: Caller,
    headers: axum::http::HeaderMap,
    Json(body): Json<CreateOperation>,
) -> Result<(StatusCode, Json<serde_json::Value>), Problem> {
    require(&caller, "operations:write")?;
    let key = headers.get("idempotency-key").and_then(|value| {
        value
            .to_str()
            .ok()
            .map(|raw| raw.trim())
            .filter(|raw| !raw.is_empty())
    });
    let Some(key) = key else {
        return Err(Problem::new(
            "missing-idempotency-key",
            StatusCode::BAD_REQUEST,
            "invalid request",
            Some("the Idempotency-Key header is required".to_owned()),
        ));
    };
    let new = fishpond_storage::NewOperation {
        id: &parse_operation_id(&body.id)?,
        tenant: &caller.tenant,
        kind: body.kind.as_str(),
        idempotency_key: key,
        at: super::now(),
    };
    let CreatedOperation {
        operation,
        replayed,
    } = state
        .store
        .create_operation(&new)
        .await
        .map_err(|err| Problem::from(&err))?;
    let status = if replayed {
        StatusCode::OK
    } else {
        StatusCode::CREATED
    };
    Ok((
        status,
        Json(serde_json::json!({
            "operation": operation_json(&operation),
            "replayed": replayed,
        })),
    ))
}

/// `GET /operations` — cursor pagination over the tenant's operations
/// (oldest first; the cursor is the offset in that ordering).
pub async fn list(
    State(state): State<AppState>,
    caller: Caller,
    Query(params): Query<BTreeMap<String, String>>,
) -> Result<Json<serde_json::Value>, Problem> {
    require(&caller, "operations:read")?;
    let limit = cursor::parse_limit(params.get("limit").map(String::as_str))?;
    let offset = match params.get("cursor") {
        Some(raw) => cursor::decode(raw)?,
        None => 0,
    };
    let all = state
        .store
        .list_operations(&caller.tenant)
        .await
        .map_err(|err| Problem::from(&err))?;
    let page = cursor::paginate(all, offset, limit);
    let items: Vec<serde_json::Value> = page.items.iter().map(operation_json).collect();
    Ok(Json(serde_json::json!({
        "operations": items,
        "next_cursor": page.next_cursor,
    })))
}

/// `GET /operations/{id}` — one operation, tenant-scoped.
pub async fn get(
    State(state): State<AppState>,
    caller: Caller,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, Problem> {
    require(&caller, "operations:read")?;
    let id = parse_operation_id(&id)?;
    let operation = state
        .store
        .get_operation(&caller.tenant, &id)
        .await
        .map_err(|err| Problem::from(&err))?;
    Ok(Json(operation_json(&operation)))
}

/// `POST /operations/{id}/claim` — CAS from PENDING to RUNNING with a
/// numbered attempt; the loser of a race gets a 409.
pub async fn claim(
    State(state): State<AppState>,
    caller: Caller,
    Path(id): Path<String>,
    Json(body): Json<ClaimOperation>,
) -> Result<Json<serde_json::Value>, Problem> {
    require(&caller, "operations:write")?;
    let id = parse_operation_id(&id)?;
    let worker = parse_worker_id(&body.worker_id)?;
    let attempt = parse_attempt_id(&body.attempt_id)?;
    let deadline = parse_deadline(body.deadline_at)?;
    let claimed = state
        .store
        .claim_operation(
            &caller.tenant,
            &id,
            &worker,
            &attempt,
            deadline,
            super::now(),
        )
        .await
        .map_err(|err| Problem::from(&err))?;
    Ok(Json(serde_json::json!({
        "operation": operation_json(&claimed.operation),
        "attempt": {
            "attempt_id": claimed.attempt.attempt_id.as_str(),
            "attempt_no": claimed.attempt.attempt_no,
            "deadline_at": claimed.attempt.deadline_at.as_millis(),
        },
    })))
}

/// `POST /operations/{id}/attempts/{attempt_id}/close` — settle one
/// attempt: `succeeded`/`failed` end the operation, `timed-out` hands
/// it back to PENDING for another claim.
pub async fn close(
    State(state): State<AppState>,
    caller: Caller,
    Path((id, attempt_id)): Path<(String, String)>,
    Json(body): Json<CloseAttempt>,
) -> Result<Json<serde_json::Value>, Problem> {
    require(&caller, "operations:write")?;
    let id = parse_operation_id(&id)?;
    let attempt_id = parse_attempt_id(&attempt_id)?;
    let worker = parse_worker_id(&body.worker_id)?;
    let close = parse_close(&body.result)?;
    let operation = state
        .store
        .close_attempt(
            &caller.tenant,
            &id,
            &attempt_id,
            &worker,
            close,
            super::now(),
        )
        .await
        .map_err(|err| Problem::from(&err))?;
    Ok(Json(operation_json(&operation)))
}
