//! The scheduler service (F013): placement execution, drain, and
//! failure takeover over the F012 lease CAS.
//!
//! [`Scheduler::assign`] snapshots the tenant's workers and live
//! leases, lets [`placement::choose`] pick a holder (capacity +
//! affinity + stickiness), and executes the acquire through the
//! storage CAS — a racing holder still wins and the caller sees the
//! conflict.
//!
//! [`Scheduler::drain`] persists the drain flag (placement refuses the
//! worker from then on), force-expires every live term the worker
//! still holds, and re-acquires each account on a replacement worker.
//! Force-expiry leaves `generation` untouched, so every migration is a
//! steal: the fencing token rises and the old holder's next renew
//! fails — it learns it lost the account instead of being silently
//! shadowed.
//!
//! [`Scheduler::take_over_lost`] is the same migration driven by
//! liveness: workers whose heartbeat crossed the staleness cutoff
//! (`Store::lost_workers`) have their live terms force-expired and
//! reassigned. [`Scheduler::drain_node`] (F037) lifts drain to the
//! deployment node: one host may run several workers under one name,
//! and the operator drains them in one call, tenant-scoped. All flows
//! report what moved, what was contested by a racing holder, and what
//! could not be placed (no eligible worker).

use fishpond_domain::{AccountId, TenantId, UnixMillis, WorkerId};
use fishpond_storage::{AccountLease, StorageError, Store, WorkerRecord};

use crate::placement::{self, PlacementPolicy, WorkerView};

/// Why a placement could not be made.
#[derive(Debug)]
pub enum SchedulerError {
    /// No worker passed the liveness, drain, affinity and capacity
    /// gates for this account.
    NoEligibleWorker {
        /// Owning tenant.
        tenant: TenantId,
        /// The account that could not be placed.
        account: AccountId,
    },
    /// No worker of this tenant is registered under the node name —
    /// either the name is a typo or the node belongs to another
    /// tenant. Never a silent success over nothing.
    UnknownNode {
        /// Owning tenant.
        tenant: TenantId,
        /// The node name that matched nothing.
        node: String,
    },
    /// The storage CAS or repository failed.
    Storage(StorageError),
}

impl std::fmt::Display for SchedulerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoEligibleWorker { tenant, account } => {
                write!(
                    f,
                    "no eligible worker for account {account} in tenant {tenant}"
                )
            }
            Self::UnknownNode { tenant, node } => {
                write!(
                    f,
                    "no workers registered for node {node} in tenant {tenant}"
                )
            }
            Self::Storage(err) => write!(f, "storage: {err}"),
        }
    }
}

impl std::error::Error for SchedulerError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Storage(err) => Some(err),
            Self::NoEligibleWorker { .. } | Self::UnknownNode { .. } => None,
        }
    }
}

impl From<StorageError> for SchedulerError {
    fn from(err: StorageError) -> Self {
        Self::Storage(err)
    }
}

/// Scheduler result alias.
pub type SchedulerResult<T> = Result<T, SchedulerError>;

/// One successful placement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Assignment {
    /// The account that was placed.
    pub account: AccountId,
    /// The worker now holding it.
    pub holder: WorkerId,
    /// The lease as acquired (fencing generation included).
    pub lease: AccountLease,
}

/// What a drain or takeover did, account by account.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MigrationReport {
    /// Workers the pass acted on (drain: the one worker; takeover:
    /// every lost worker found).
    pub sources: Vec<WorkerId>,
    /// Accounts successfully moved: `(account, new holder, new
    /// generation)` — the generation rose, so the old holder's next
    /// renew fails loudly.
    pub moved: Vec<(AccountId, WorkerId, i64)>,
    /// Accounts whose re-acquire lost a CAS race to another holder;
    /// they are still leased, just not by the worker we picked. The
    /// next pass re-checks them.
    pub contested: Vec<AccountId>,
    /// Accounts no eligible worker could take (everyone draining,
    /// lost, at capacity, or lacking the capabilities).
    pub unassigned: Vec<AccountId>,
}

/// What [`Scheduler::drain`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DrainReport {
    /// The per-account outcome.
    pub migration: MigrationReport,
    /// Whether the worker was already draining before this call
    /// (drain is idempotent).
    pub already_draining: bool,
}

/// What [`Scheduler::take_over_lost`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TakeoverReport {
    /// The per-account outcome.
    pub migration: MigrationReport,
}

/// What [`Scheduler::drain_node`] did across the node's workers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeDrainReport {
    /// The node name that was drained.
    pub node: String,
    /// The tenant's workers registered under that name — every one of
    /// them drained.
    pub workers: Vec<WorkerId>,
    /// The per-account outcome across the whole node.
    pub migration: MigrationReport,
    /// How many members were already draining before this call (drain
    /// is idempotent per worker).
    pub already_draining: usize,
}

/// The capacity- and affinity-aware scheduler.
#[derive(Clone)]
pub struct Scheduler {
    store: Store,
    policy: PlacementPolicy,
}

impl Scheduler {
    /// Builds a scheduler over an opened [`Store`].
    pub fn new(store: Store, policy: PlacementPolicy) -> Self {
        Self { store, policy }
    }

    /// The configured policy.
    pub fn policy(&self) -> &PlacementPolicy {
        &self.policy
    }

    /// Places `account` on an eligible worker, preferring `prefer`
    /// (its current holder, when re-placing) and acquiring the lease
    /// until `at + term_ms`. Fails with
    /// [`SchedulerError::NoEligibleWorker`] when no worker passes the
    /// gates; a CAS race against a concurrent holder surfaces as a
    /// storage conflict naming the winner.
    pub async fn assign(
        &self,
        tenant: &TenantId,
        account: &AccountId,
        required: &[String],
        prefer: Option<&WorkerId>,
        term_ms: i64,
        at: UnixMillis,
    ) -> SchedulerResult<Assignment> {
        let views = self.worker_views(tenant, at).await?;
        let Some(worker) = placement::choose(&views, required, at, &self.policy, prefer) else {
            return Err(SchedulerError::NoEligibleWorker {
                tenant: tenant.clone(),
                account: account.clone(),
            });
        };
        let expires_at = UnixMillis::from_millis(at.as_millis().saturating_add(term_ms))
            .map_err(|err| SchedulerError::Storage(StorageError::Domain(err)))?;
        let lease = self
            .store
            .acquire_lease(tenant, account, &worker.id, expires_at, at)
            .await?;
        Ok(Assignment {
            account: account.clone(),
            holder: worker.id.clone(),
            lease,
        })
    }

    /// Drains `worker`: marks it draining (idempotent), force-expires
    /// its live terms and migrates each account to an eligible
    /// replacement. The worker keeps its registration row — a drained
    /// worker that is still heartbeating simply receives no new work
    /// and loses its existing work; deregistration stays an explicit
    /// operator action.
    pub async fn drain(
        &self,
        tenant: &TenantId,
        worker: &WorkerId,
        term_ms: i64,
        at: UnixMillis,
    ) -> SchedulerResult<DrainReport> {
        let already_draining = self.drain_mark(tenant, worker, at).await?;
        let accounts = self.live_accounts_of(tenant, worker, at).await?;
        let migration = self.migrate_off(tenant, &accounts, term_ms, at).await;
        Ok(DrainReport {
            migration,
            already_draining,
        })
    }

    /// Marks `worker` draining unless it already is (idempotent);
    /// returns whether it was already draining before this call.
    async fn drain_mark(
        &self,
        tenant: &TenantId,
        worker: &WorkerId,
        at: UnixMillis,
    ) -> SchedulerResult<bool> {
        let current = self.store.get_worker(tenant, worker).await?;
        if !current.draining {
            self.store.drain_worker(tenant, worker, at).await?;
        }
        Ok(current.draining)
    }

    /// Drains a deployment node (F037): every worker of the tenant
    /// registered under `node` (one host runs several worker
    /// processes) goes away together. Every member is marked draining
    /// *first* — placement then refuses the whole node before any
    /// migration runs, so an account leaving one member can never land
    /// on a not-yet-drained sibling of the same machine. A name that
    /// matches no worker of this tenant is
    /// [`SchedulerError::UnknownNode`] — the lookup is tenant-scoped,
    /// so another tenant's same-named host is invisible and an
    /// operator typo cannot silently drain nothing.
    pub async fn drain_node(
        &self,
        tenant: &TenantId,
        node: &str,
        term_ms: i64,
        at: UnixMillis,
    ) -> SchedulerResult<NodeDrainReport> {
        let members: Vec<WorkerId> = self
            .store
            .list_workers(tenant)
            .await?
            .into_iter()
            .filter(|worker| worker.name == node)
            .map(|worker| worker.id)
            .collect();
        if members.is_empty() {
            return Err(SchedulerError::UnknownNode {
                tenant: tenant.clone(),
                node: node.to_owned(),
            });
        }
        let mut already_draining = 0usize;
        for worker in &members {
            if self.drain_mark(tenant, worker, at).await? {
                already_draining += 1;
            }
        }
        let mut migration = MigrationReport {
            sources: members.clone(),
            moved: Vec::new(),
            contested: Vec::new(),
            unassigned: Vec::new(),
        };
        for worker in &members {
            let accounts = self.live_accounts_of(tenant, worker, at).await?;
            migration.merge(self.migrate_off(tenant, &accounts, term_ms, at).await);
        }
        Ok(NodeDrainReport {
            node: node.to_owned(),
            workers: members,
            migration,
            already_draining,
        })
    }

    /// Takes over every lost worker's accounts: workers whose last
    /// heartbeat is at or before `at - staleness_ms` have their live
    /// terms force-expired and reassigned to healthy workers. Workers
    /// that are already draining but not lost are left alone — drain
    /// has its own flow.
    pub async fn take_over_lost(
        &self,
        tenant: &TenantId,
        staleness_ms: i64,
        term_ms: i64,
        at: UnixMillis,
    ) -> SchedulerResult<TakeoverReport> {
        let cutoff = UnixMillis::from_millis(at.as_millis().saturating_sub(staleness_ms))
            .map_err(|err| SchedulerError::Storage(StorageError::Domain(err)))?;
        let lost = self.store.lost_workers(tenant, cutoff).await?;
        let mut migration = MigrationReport {
            sources: lost.iter().map(|w| w.id.clone()).collect(),
            moved: Vec::new(),
            contested: Vec::new(),
            unassigned: Vec::new(),
        };
        for worker in &lost {
            let accounts = self.live_accounts_of(tenant, &worker.id, at).await?;
            migration.merge(self.migrate_off(tenant, &accounts, term_ms, at).await);
        }
        Ok(TakeoverReport { migration })
    }

    /// Every account `worker` currently holds a live lease on.
    async fn live_accounts_of(
        &self,
        tenant: &TenantId,
        worker: &WorkerId,
        at: UnixMillis,
    ) -> SchedulerResult<Vec<AccountId>> {
        let live = self.store.list_live_leases(tenant, at).await?;
        Ok(live
            .into_iter()
            .filter(|lease| &lease.holder == worker)
            .map(|lease| lease.account_id)
            .collect())
    }

    /// Force-expires the given accounts' terms on their current holder
    /// and re-acquires each on the best eligible worker. The snapshot
    /// for the replacement choice is taken *after* the force-expiry, so
    /// a draining or lost source worker is excluded by the placement
    /// gates themselves.
    async fn migrate_off(
        &self,
        tenant: &TenantId,
        accounts: &[AccountId],
        term_ms: i64,
        at: UnixMillis,
    ) -> MigrationReport {
        let mut report = MigrationReport {
            sources: Vec::new(),
            moved: Vec::new(),
            contested: Vec::new(),
            unassigned: Vec::new(),
        };
        if accounts.is_empty() {
            return report;
        }
        // Resolve each account's current holder; unreadable accounts
        // sit out this pass.
        let mut movable: Vec<(AccountId, WorkerId)> = Vec::new();
        for account in accounts {
            match self.store.get_lease(tenant, account).await {
                Ok(Some(lease)) => movable.push((account.clone(), lease.holder)),
                Ok(None) => report.unassigned.push(account.clone()),
                Err(_) => report.unassigned.push(account.clone()),
            }
        }
        // One expiry cut per distinct holder; the holder filter makes
        // it hit exactly these accounts' live terms.
        let mut holders: Vec<WorkerId> = Vec::new();
        for (_, holder) in &movable {
            if !holders.contains(holder) {
                holders.push(holder.clone());
            }
        }
        for holder in &holders {
            if self
                .store
                .force_expire_leases(tenant, holder, at)
                .await
                .is_err()
            {
                // Cannot cut the terms: nothing is safely movable this
                // pass; leave every account unassigned rather than
                // assigning through a half-cut state.
                report
                    .unassigned
                    .extend(movable.into_iter().map(|(account, _)| account));
                return report;
            }
        }
        for (account, _) in &movable {
            // Affinity-neutral migration: the account-level
            // requirements are the caller's to re-evaluate (see module
            // docs); any capacity-eligible live worker may take over.
            let views = match self.worker_views(tenant, at).await {
                Ok(views) => views,
                Err(_) => {
                    report.unassigned.push(account.clone());
                    continue;
                }
            };
            let Some(worker) = placement::choose(&views, &[], at, &self.policy, None) else {
                report.unassigned.push(account.clone());
                continue;
            };
            let expires_at = UnixMillis::from_millis(at.as_millis().saturating_add(term_ms))
                .expect("term keeps the instant non-negative");
            match self
                .store
                .acquire_lease(tenant, account, &worker.id, expires_at, at)
                .await
            {
                Ok(lease) => {
                    report
                        .moved
                        .push((account.clone(), worker.id.clone(), lease.generation));
                }
                Err(StorageError::Conflict { .. }) => report.contested.push(account.clone()),
                Err(_) => report.unassigned.push(account.clone()),
            }
        }
        report
    }

    /// Fresh placement snapshot: every worker of the tenant with its
    /// live lease count.
    async fn worker_views(
        &self,
        tenant: &TenantId,
        at: UnixMillis,
    ) -> SchedulerResult<Vec<WorkerView>> {
        let workers: Vec<WorkerRecord> = self.store.list_workers(tenant).await?;
        let leases = self.store.list_live_leases(tenant, at).await?;
        let mut counts: Vec<(&WorkerId, usize)> = Vec::new();
        for lease in &leases {
            match counts.iter_mut().find(|(id, _)| **id == lease.holder) {
                Some((_, count)) => *count += 1,
                None => counts.push((&lease.holder, 1)),
            }
        }
        Ok(workers
            .into_iter()
            .map(|w| WorkerView {
                lease_count: counts
                    .iter()
                    .find(|(id, _)| **id == w.id)
                    .map(|(_, count)| *count)
                    .unwrap_or(0),
                id: w.id,
                capabilities: w.capabilities,
                draining: w.draining,
                last_heartbeat_at: w.last_heartbeat_at,
                registered_at: w.registered_at,
            })
            .collect())
    }
}

impl MigrationReport {
    /// Folds another report into this one (takeover across workers).
    fn merge(&mut self, other: MigrationReport) {
        self.sources.extend(other.sources);
        self.moved.extend(other.moved);
        self.contested.extend(other.contested);
        self.unassigned.extend(other.unassigned);
    }
}
