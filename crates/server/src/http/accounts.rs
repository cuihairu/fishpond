//! Account routes (F025): the REST mapping of [`Accounts`]. Reads
//! need `accounts:read`, writes need `accounts:write`; the tenant is
//! always the caller's (a key never sees another tenant's rows), and
//! every failure is a Problem document.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};

use fishpond_domain::{Account, AccountId, AccountState};

use super::AppState;
use super::auth::Caller;
use super::problem::Problem;
use crate::accounts::StatusSummary;

/// The account wire DTO.
#[derive(Debug, Serialize)]
pub struct AccountDto {
    /// Account id.
    pub id: String,
    /// Platform identifier.
    pub platform: String,
    /// State-machine state (canonical name).
    pub state: &'static str,
    /// The UNKNOWN note, when present.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub state_note: Option<String>,
    /// Last update instant (unix ms).
    pub updated_at: i64,
}

impl From<&Account> for AccountDto {
    fn from(account: &Account) -> Self {
        AccountDto {
            id: account.id.as_str().to_owned(),
            platform: account.platform.clone(),
            state: account.state.as_str(),
            state_note: account.state_note.clone(),
            updated_at: account.updated_at.as_millis(),
        }
    }
}

impl From<Account> for AccountDto {
    fn from(account: Account) -> Self {
        AccountDto::from(&account)
    }
}

/// The status wire DTO.
#[derive(Debug, Serialize)]
pub struct StatusDto {
    /// Confirmed online accounts.
    pub online: usize,
    /// Confirmed offline accounts.
    pub offline: usize,
    /// Could not be verified.
    pub unknown: usize,
    /// Administratively disabled.
    pub disabled: usize,
    /// Total.
    pub total: usize,
}

impl From<StatusSummary> for StatusDto {
    fn from(summary: StatusSummary) -> Self {
        StatusDto {
            online: summary.online,
            offline: summary.offline,
            unknown: summary.unknown,
            disabled: summary.disabled,
            total: summary.total,
        }
    }
}

/// `POST /accounts` request body.
#[derive(Debug, Deserialize)]
pub struct RegisterAccount {
    /// The new account's id.
    pub id: String,
    /// Platform identifier.
    pub platform: String,
}

/// `POST /accounts/{id}/state` request body.
#[derive(Debug, Deserialize)]
pub struct SetAccountState {
    /// The target state (canonical name).
    pub state: String,
    /// Optional note (required by the domain for UNKNOWN).
    #[serde(default)]
    pub note: Option<String>,
}

/// Denies the call unless the caller's key grants `scope`.
pub fn require(caller: &Caller, scope: &str) -> Result<(), Problem> {
    if caller.has(scope) {
        Ok(())
    } else {
        Err(Problem::new(
            "scope-denied",
            StatusCode::FORBIDDEN,
            "scope denied",
            Some(format!("this call requires the {scope} scope")),
        ))
    }
}

fn parse_state(raw: &str) -> Result<AccountState, Problem> {
    AccountState::parse(raw).map_err(|err| {
        Problem::new(
            "invalid",
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid request",
            Some(err.to_string()),
        )
    })
}

fn parse_account_id(raw: &str) -> Result<AccountId, Problem> {
    AccountId::parse(raw.to_owned()).map_err(|err| {
        Problem::new(
            "invalid",
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid request",
            Some(format!("account id: {err}")),
        )
    })
}

/// `GET /accounts` — every account of the caller's tenant, ordered by
/// id (the storage layer's ordering; stable for pagination).
pub async fn list(
    State(state): State<AppState>,
    caller: Caller,
) -> Result<Json<serde_json::Value>, Problem> {
    require(&caller, "accounts:read")?;
    let accounts = state
        .accounts
        .list(&caller.tenant)
        .await
        .map_err(|err| Problem::from(&err))?;
    let accounts: Vec<AccountDto> = accounts.iter().map(AccountDto::from).collect();
    Ok(Json(serde_json::json!({ "accounts": accounts })))
}

/// `POST /accounts` — registers an account (201 with the DTO).
pub async fn register(
    State(state): State<AppState>,
    caller: Caller,
    Json(body): Json<RegisterAccount>,
) -> Result<(StatusCode, Json<AccountDto>), Problem> {
    require(&caller, "accounts:write")?;
    let id = parse_account_id(&body.id)?;
    let account = state
        .accounts
        .register(&caller.tenant, id, &body.platform, super::now())
        .await
        .map_err(|err| Problem::from(&err))?;
    Ok((StatusCode::CREATED, Json(AccountDto::from(account))))
}

/// `GET /accounts/{id}` — one account (404 when absent or foreign).
pub async fn get(
    State(state): State<AppState>,
    caller: Caller,
    Path(id): Path<String>,
) -> Result<Json<AccountDto>, Problem> {
    require(&caller, "accounts:read")?;
    let account_id = parse_account_id(&id)?;
    let account = state
        .accounts
        .get(&caller.tenant, &account_id)
        .await
        .map_err(|err| Problem::from(&err))?;
    Ok(Json(AccountDto::from(account)))
}

/// `POST /accounts/{id}/state` — one state-machine step.
pub async fn set_state(
    State(state): State<AppState>,
    caller: Caller,
    Path(id): Path<String>,
    Json(body): Json<SetAccountState>,
) -> Result<Json<AccountDto>, Problem> {
    require(&caller, "accounts:write")?;
    let account_id = parse_account_id(&id)?;
    let next = parse_state(&body.state)?;
    let account = state
        .accounts
        .set_state(&caller.tenant, &account_id, next, body.note, super::now())
        .await
        .map_err(|err| Problem::from(&err))?;
    Ok(Json(AccountDto::from(account)))
}

/// The deletion summary wire DTO (F040): what one account deletion
/// removed, counted per table.
#[derive(Debug, Serialize)]
pub struct DeletedAccountDto {
    /// Deleted conversations (their messages count separately).
    pub conversations: u64,
    /// Deleted messages.
    pub messages: u64,
    /// Deleted takeover grants.
    pub grants: u64,
    /// Deleted authorization sessions.
    pub sessions: u64,
    /// Deleted account leases.
    pub leases: u64,
    /// Deleted profiles.
    pub profiles: u64,
}

impl From<fishpond_storage::DeletedAccount> for DeletedAccountDto {
    fn from(removed: fishpond_storage::DeletedAccount) -> Self {
        DeletedAccountDto {
            conversations: removed.conversations,
            messages: removed.messages,
            grants: removed.grants,
            sessions: removed.sessions,
            leases: removed.leases,
            profiles: removed.profiles,
        }
    }
}

/// `DELETE /accounts/{id}` — removes the account with every session,
/// conversation, message, takeover grant, lease and profile in one
/// transaction. Observation history survives as the audit trail and an
/// `account.deleted` event records the counts; 404 when the id is
/// absent or foreign.
pub async fn delete(
    State(state): State<AppState>,
    caller: Caller,
    Path(id): Path<String>,
) -> Result<Json<DeletedAccountDto>, Problem> {
    require(&caller, "accounts:write")?;
    let account_id = parse_account_id(&id)?;
    let removed = state
        .accounts
        .delete(&caller.tenant, &account_id, super::now())
        .await
        .map_err(|err| Problem::from(&err))?;
    Ok(Json(DeletedAccountDto::from(removed)))
}

/// `GET /status` — the tenant's account counts by state.
pub async fn status(
    State(state): State<AppState>,
    caller: Caller,
) -> Result<Json<StatusDto>, Problem> {
    require(&caller, "accounts:read")?;
    let summary = state
        .accounts
        .status(&caller.tenant)
        .await
        .map_err(|err| Problem::from(&err))?;
    Ok(Json(StatusDto::from(summary)))
}
