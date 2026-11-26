//! Account observation audit trail (F015): one appended row per
//! reconcile pass or manual confirmation.
//!
//! The accounts row is the live projection of the state machine; this
//! table is its history. Domain invariants keep notes off confirmed
//! account states, so the audit context (probe reason, operator,
//! justification) lives here instead — see [`ObservationRecord`].
//!
//! Every surface is tenant-bound via [`Store::begin_tenant`], with
//! row-level security as a second isolation layer.

use fishpond_domain::{AccountId, TenantId, UnixMillis};
use sqlx::Row;

use crate::error::{StorageError, StorageResult};
use crate::store::Store;

/// Where an observation came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObservationSource {
    /// An automatic reconcile probe.
    Reconcile,
    /// A human decision (manual verification).
    Manual,
}

impl ObservationSource {
    /// Canonical wire name stored in SQL.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Reconcile => "reconcile",
            Self::Manual => "manual",
        }
    }

    /// Parses the canonical wire name.
    pub fn parse(raw: &str) -> StorageResult<Self> {
        match raw {
            "reconcile" => Ok(Self::Reconcile),
            "manual" => Ok(Self::Manual),
            other => Err(StorageError::Invalid {
                what: "observation source",
                reason: format!("unknown source {other}"),
            }),
        }
    }
}

/// What an observation did to the account.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObservationOutcome {
    /// The observed state was applied (a transition happened).
    Applied,
    /// The probe confirmed the state the account already had (or the
    /// operator confirmed it): no state change.
    Unchanged,
    /// The account was skipped (administratively disabled) — recorded
    /// so a silent skip is distinguishable from a missed scan.
    Skipped,
}

impl ObservationOutcome {
    /// Canonical wire name stored in SQL.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Applied => "applied",
            Self::Unchanged => "unchanged",
            Self::Skipped => "skipped",
        }
    }

    /// Parses the canonical wire name.
    pub fn parse(raw: &str) -> StorageResult<Self> {
        match raw {
            "applied" => Ok(Self::Applied),
            "unchanged" => Ok(Self::Unchanged),
            "skipped" => Ok(Self::Skipped),
            other => Err(StorageError::Invalid {
                what: "observation outcome",
                reason: format!("unknown outcome {other}"),
            }),
        }
    }
}

/// One appended observation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObservationRecord {
    /// Observation id.
    pub id: String,
    /// Owning tenant.
    pub tenant_id: TenantId,
    /// The observed account.
    pub account_id: AccountId,
    /// Automatic probe or human decision.
    pub source: ObservationSource,
    /// The state observed/confirmed (wire name; `UNKNOWN` counts).
    pub observed: String,
    /// What happened to the account.
    pub outcome: ObservationOutcome,
    /// Probe reason or operator justification (NULL when none).
    pub note: Option<String>,
    /// The human who confirmed (NULL for automatic probes).
    pub operator: Option<String>,
    /// When the observation was recorded.
    pub at: UnixMillis,
}

/// An observation to append.
pub struct NewObservation<'a> {
    /// Observation id (caller-generated, unique per tenant).
    pub id: &'a str,
    /// Owning tenant.
    pub tenant: &'a TenantId,
    /// The observed account.
    pub account: &'a AccountId,
    /// Automatic probe or human decision.
    pub source: ObservationSource,
    /// The state observed/confirmed (wire name).
    pub observed: &'a str,
    /// What happened to the account.
    pub outcome: ObservationOutcome,
    /// Probe reason or operator justification.
    pub note: Option<&'a str>,
    /// The human who confirmed (automatic probes pass `None`).
    pub operator: Option<&'a str>,
    /// When the observation was recorded.
    pub at: UnixMillis,
}

impl Store {
    /// Appends one observation row. The account must exist and belong
    /// to the tenant; the note must be present when `observed` is
    /// `UNKNOWN` (mirroring the domain invariant on the live row).
    pub async fn record_observation(
        &self,
        new: &NewObservation<'_>,
    ) -> StorageResult<ObservationRecord> {
        if new.observed == "UNKNOWN" && new.note.is_none_or(|note| note.trim().is_empty()) {
            return Err(StorageError::Invalid {
                what: "observation note",
                reason: "required while the observed state is UNKNOWN".to_owned(),
            });
        }
        let mut tx = self.begin_tenant(new.tenant).await?;
        sqlx::query(
            "INSERT INTO account_observations
                 (id, tenant_id, account_id, source, observed, outcome, note, operator, at)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
        )
        .bind(new.id)
        .bind(new.tenant.as_str())
        .bind(new.account.as_str())
        .bind(new.source.as_str())
        .bind(new.observed)
        .bind(new.outcome.as_str())
        .bind(new.note)
        .bind(new.operator)
        .bind(new.at.as_millis())
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(ObservationRecord {
            id: new.id.to_owned(),
            tenant_id: new.tenant.clone(),
            account_id: new.account.clone(),
            source: new.source,
            observed: new.observed.to_owned(),
            outcome: new.outcome,
            note: new.note.map(str::to_owned),
            operator: new.operator.map(str::to_owned),
            at: new.at,
        })
    }

    /// Lists an account's observations, newest first (id breaks ties).
    pub async fn list_observations(
        &self,
        tenant: &TenantId,
        account: &AccountId,
    ) -> StorageResult<Vec<ObservationRecord>> {
        let mut tx = self.begin_tenant(tenant).await?;
        let rows = sqlx::query(
            "SELECT id, tenant_id, account_id, source, observed, outcome, note, operator, at
             FROM account_observations WHERE account_id = $1
             ORDER BY at DESC, id DESC",
        )
        .bind(account.as_str())
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;
        rows.iter().map(map_observation).collect()
    }
}

fn map_observation(row: &sqlx::postgres::PgRow) -> StorageResult<ObservationRecord> {
    Ok(ObservationRecord {
        id: row.try_get("id")?,
        tenant_id: TenantId::parse(row.try_get::<String, _>("tenant_id")?)?,
        account_id: AccountId::parse(row.try_get::<String, _>("account_id")?)?,
        source: ObservationSource::parse(row.try_get::<String, _>("source")?.as_str())?,
        observed: row.try_get("observed")?,
        outcome: ObservationOutcome::parse(row.try_get::<String, _>("outcome")?.as_str())?,
        note: row.try_get("note")?,
        operator: row.try_get("operator")?,
        at: UnixMillis::from_millis(row.try_get::<i64, _>("at")?)?,
    })
}
