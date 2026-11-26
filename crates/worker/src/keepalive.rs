//! Worker keep-alive daemon (F013): the heartbeat + lease renewal loop
//! that F012 deferred to the scheduler milestone.
//!
//! A healthy worker must do two things on an interval: heartbeat its
//! registry row and renew every lease it holds *before the term
//! expires* — [`Store::renew_lease`] refuses to extend an expired
//! term, so a worker that sleeps past its term has to go through the
//! steal path again.
//!
//! The steal-back policy is deliberately narrow: on a failed renewal
//! the worker tries to take its own account back exactly once. If the
//! term merely expired while it slept (nobody else grabbed it), the
//! re-acquire re-extends its own lease and the fencing generation does
//! not move — it never lost the account. If another worker already
//! stole it, the CAS names the new holder and this worker gives up:
//! stealing a *live* foreign lease is impossible by design, which is
//! what makes the generation a sound fencing token.
//!
//! [`KeepAlive::tick`] is the whole policy driven by an explicit
//! instant, so tests are deterministic; [`KeepAlive::spawn`] is the
//! thin wall-clock wrapper that calls `tick` on an interval.

use fishpond_domain::{AccountId, TenantId, UnixMillis, WorkerId};
use fishpond_storage::{StorageError, Store};

/// Keep-alive knobs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeepAliveConfig {
    /// Lease term the worker keeps renewing to (`now + term_ms`).
    pub term_ms: i64,
}

impl KeepAliveConfig {
    /// Builds a config, rejecting non-positive terms.
    pub fn new(term_ms: i64) -> Option<Self> {
        if term_ms <= 0 {
            None
        } else {
            Some(Self { term_ms })
        }
    }
}

/// What happened to one account's lease in a tick.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RenewOutcome {
    /// The term was extended in place; the fencing generation is
    /// unchanged.
    Renewed {
        /// The fencing token (unchanged by a renewal).
        generation: i64,
    },
    /// The renewal failed but the steal-back re-acquire succeeded.
    /// `generation` is the fencing token after re-acquiring: unchanged
    /// when nobody else had taken the account, risen when the worker
    /// had to steal its own expired term back.
    Regained {
        /// The fencing token after the steal-back.
        generation: i64,
    },
    /// The account is gone: another worker holds the live lease. The
    /// holder is named for diagnostics.
    LostTo {
        /// The worker that now holds the account.
        holder: WorkerId,
    },
    /// The storage layer failed; the account's fate this tick is
    /// unknown and the next tick retries.
    StorageFault,
}

/// The result of one keep-alive tick.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TickReport {
    /// Whether the registry heartbeat landed (`false` when the worker
    /// row is gone — the caller decides whether that is fatal).
    pub heartbeat_ok: bool,
    /// Per-account renewal outcomes, in the accounts' declared order.
    pub accounts: Vec<(AccountId, RenewOutcome)>,
}

/// The keep-alive service for one worker and the accounts it holds.
#[derive(Clone)]
pub struct KeepAlive {
    store: Store,
    tenant: TenantId,
    worker: WorkerId,
    accounts: Vec<AccountId>,
    config: KeepAliveConfig,
}

impl KeepAlive {
    /// Builds the service for `worker` holding `accounts`.
    pub fn new(
        store: Store,
        tenant: TenantId,
        worker: WorkerId,
        accounts: Vec<AccountId>,
        config: KeepAliveConfig,
    ) -> Self {
        Self {
            store,
            tenant,
            worker,
            accounts,
            config,
        }
    }

    /// The worker this service keeps alive.
    pub fn worker(&self) -> &WorkerId {
        &self.worker
    }

    /// Runs one keep-alive step at `now`: heartbeat, then renew every
    /// account (steal-back on failure). Deterministic in `now`.
    pub async fn tick(&self, now: UnixMillis) -> fishpond_storage::StorageResult<TickReport> {
        let heartbeat_ok = self
            .store
            .heartbeat_worker(&self.tenant, &self.worker, now)
            .await?;
        let expires_at =
            UnixMillis::from_millis(now.as_millis().saturating_add(self.config.term_ms))
                .map_err(StorageError::Domain)?;
        let mut accounts = Vec::with_capacity(self.accounts.len());
        for account in &self.accounts {
            accounts.push((
                account.clone(),
                self.renew_one(account, expires_at, now).await,
            ));
        }
        Ok(TickReport {
            heartbeat_ok,
            accounts,
        })
    }

    /// Renews one lease; on failure, attempts the one-shot steal-back.
    async fn renew_one(
        &self,
        account: &AccountId,
        expires_at: UnixMillis,
        now: UnixMillis,
    ) -> RenewOutcome {
        match self
            .store
            .renew_lease(&self.tenant, account, &self.worker, expires_at, now)
            .await
        {
            Ok(lease) => RenewOutcome::Renewed {
                generation: lease.generation,
            },
            // Live foreign holder: the steal-back cannot and must not
            // win; report who took the account.
            Err(StorageError::Conflict { reason, .. }) if reason.contains("held by") => {
                match self.store.get_lease(&self.tenant, account).await {
                    Ok(Some(lease)) => RenewOutcome::LostTo {
                        holder: lease.holder,
                    },
                    _ => RenewOutcome::StorageFault,
                }
            }
            // Our own term expired (or the row vanished): take it back
            // once through the acquire CAS.
            Err(StorageError::Conflict { .. }) | Err(StorageError::NotFound { .. }) => {
                match self
                    .store
                    .acquire_lease(&self.tenant, account, &self.worker, expires_at, now)
                    .await
                {
                    Ok(lease) => RenewOutcome::Regained {
                        generation: lease.generation,
                    },
                    // The CAS lost to a live foreign holder in the
                    // instant between renew and acquire.
                    Err(StorageError::Conflict { .. }) => {
                        match self.store.get_lease(&self.tenant, account).await {
                            Ok(Some(lease)) => RenewOutcome::LostTo {
                                holder: lease.holder,
                            },
                            _ => RenewOutcome::StorageFault,
                        }
                    }
                    Err(_) => RenewOutcome::StorageFault,
                }
            }
            Err(_) => RenewOutcome::StorageFault,
        }
    }

    /// Spawns the wall-clock loop: `tick` every `interval` until the
    /// task is aborted (via the returned handle). Tick failures are
    /// swallowed — the next tick retries — so a transient storage
    /// fault never kills the daemon. Per-tick outcome reporting is the
    /// caller's to add on top of [`KeepAlive::tick`]; the loop itself
    /// only keeps the worker alive.
    pub fn spawn(self, interval: std::time::Duration) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(interval);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            // The first tick fires immediately, matching "start
            // keeping alive right away".
            loop {
                ticker.tick().await;
                let millis = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .expect("clock is after the epoch")
                    .as_millis() as i64;
                if let Ok(now) = UnixMillis::from_millis(millis) {
                    let _ = self.tick(now).await;
                }
            }
        })
    }
}
