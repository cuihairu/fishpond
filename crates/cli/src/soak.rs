//! F039 soak: the simulated-stability churn driver behind
//! `fishpond soak`. Time is *simulated*: the driver advances a virtual
//! clock one tick per iteration, so a 72-hour horizon runs in minutes
//! of real time over the real database, the real scheduler (placement,
//! drain, takeover) and the real lease fencing and operation state
//! machine. Every tick injects seeded churn — node drains, worker
//! deaths recovered by takeover, operation claim/close with mixed
//! outcomes — and re-checks the safety invariants:
//!
//! * no live lease sits on a draining or dead worker,
//! * no worker holds past its capacity,
//! * a lease's fencing generation never moves backwards.
//!
//! Any violation is reported (and exits 1); the run itself is
//! deterministic under its `--seed`. Reports are secret-free: the
//! database URL is never echoed.

use std::collections::{BTreeMap, BTreeSet};
use std::process::ExitCode;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use fishpond_domain::{
    Account, AccountId, AccountState, AttemptId, OperationId, TenantId, UnixMillis, WorkerId,
};
use fishpond_scheduler::{PlacementPolicy, Scheduler, SchedulerError};
use fishpond_storage::{AttemptClose, NewOperation, NewWorker, Store};

/// The churn knobs (all tunable from the CLI).
#[derive(Debug, Clone)]
pub struct SoakConfig {
    /// The simulated horizon (virtual milliseconds).
    pub sim_duration_ms: i64,
    /// Virtual milliseconds per tick.
    pub tick_ms: i64,
    /// Deployment nodes (each a distinct worker `name`).
    pub nodes: usize,
    /// Worker processes per node.
    pub workers_per_node: usize,
    /// Automation accounts competing for placement.
    pub accounts: usize,
    /// Placement capacity per worker.
    pub capacity: usize,
    /// Lease term handed out by every placement.
    pub term_ms: i64,
    /// The churn seed (deterministic runs).
    pub seed: u64,
    /// Real-time hard stop (ms) — a runaway loop cannot wedge the
    /// operator's terminal.
    pub real_cap_ms: u64,
}

impl Default for SoakConfig {
    fn default() -> Self {
        Self {
            sim_duration_ms: 3_600_000,
            tick_ms: 30_000,
            nodes: 2,
            workers_per_node: 2,
            accounts: 6,
            capacity: 2,
            term_ms: 120_000,
            seed: 0xF039,
            real_cap_ms: 120_000,
        }
    }
}

impl SoakConfig {
    /// An all-zero report shaped like this run's outcome.
    pub fn empty_report(&self) -> SoakReport {
        SoakReport {
            ticks: 0,
            assigns: 0,
            assigns_failed: 0,
            moved: 0,
            contested: 0,
            unassigned: 0,
            node_drains: 0,
            worker_deaths: 0,
            revives: 0,
            operations_claimed: 0,
            operations_closed: 0,
            violations: Vec::new(),
            time_capped: false,
            sim_elapsed_ms: 0,
            real_elapsed_ms: 0,
        }
    }
}

/// What the soak did, and every invariant that broke (empty = clean).
#[derive(Debug, Clone)]
pub struct SoakReport {
    /// Ticks executed (one per simulated `tick_ms` step).
    pub ticks: u64,
    /// Successful placements.
    pub assigns: u64,
    /// Placements refused because no worker was eligible (full
    /// capacity / everyone draining or dead).
    pub assigns_failed: u64,
    /// Accounts migrated by drains and takeovers.
    pub moved: u64,
    /// Migrations that lost a CAS race.
    pub contested: u64,
    /// Migrations with no eligible target.
    pub unassigned: u64,
    /// Node drains fired.
    pub node_drains: u64,
    /// Worker deaths (stale-heartbeat churn recovered by takeover).
    pub worker_deaths: u64,
    /// Dead workers revived (heartbeat resumed).
    pub revives: u64,
    /// Operations claimed.
    pub operations_claimed: u64,
    /// Attempts closed (succeeded / failed / timed out).
    pub operations_closed: u64,
    /// Every invariant breach seen (the run's whole point: none).
    pub violations: Vec<String>,
    /// The real-time cap tripped before the horizon completed.
    pub time_capped: bool,
    /// Simulated milliseconds elapsed.
    pub sim_elapsed_ms: i64,
    /// Real milliseconds elapsed.
    pub real_elapsed_ms: u128,
}

/// The secret-free one-line report (violations follow on their own
/// lines).
pub fn render(report: &SoakReport) -> String {
    let mut out = format!(
        "soak: ticks={} sim={}ms real={}ms assigns={}/{} moved={} contested={} unassigned={} \
         drains={} deaths={} revives={} ops={}/{} violations={}",
        report.ticks,
        report.sim_elapsed_ms,
        report.real_elapsed_ms,
        report.assigns,
        report.assigns_failed,
        report.moved,
        report.contested,
        report.unassigned,
        report.node_drains,
        report.worker_deaths,
        report.revives,
        report.operations_claimed,
        report.operations_closed,
        report.violations.len(),
    );
    for violation in &report.violations {
        out.push_str("\nviolation: ");
        out.push_str(violation);
    }
    if report.time_capped {
        out.push_str("\nwarning: real-time cap hit before the horizon completed");
    }
    out
}

/// Deterministic xorshift64* — no external dependency, stable runs.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, bound: u64) -> u64 {
        if bound == 0 { 0 } else { self.next() % bound }
    }

    fn chance(&mut self, percent: u64) -> bool {
        self.below(100) < percent
    }
}

/// One fleet member.
struct Fleet {
    id: WorkerId,
    node: usize,
    dead_until: Option<i64>,
    /// The simulated instant this member's heartbeat became stale —
    /// from here on a takeover pass must have cleaned it; a live lease
    /// past this point is an invariant breach (before it, the
    /// staleness grace window legitimately keeps it eligible).
    stale_at: i64,
}

/// Runs the soak against `tenant` (which must exist — the caller owns
/// tenant provisioning, same discipline as `purge`). The fleet and the
/// operation ids are unique per run, so repeated soaks on one tenant
/// never collide with earlier rows.
pub async fn run_soak(store: &Store, tenant: &str, cfg: &SoakConfig) -> Result<SoakReport, String> {
    let tenant = TenantId::parse(tenant.to_owned()).map_err(|e| format!("tenant: {e}"))?;
    if cfg.tick_ms <= 0 {
        return Err("tick_ms must be positive".to_owned());
    }
    if cfg.capacity == 0 || cfg.nodes == 0 || cfg.workers_per_node == 0 || cfg.accounts == 0 {
        return Err("nodes, workers-per-node, accounts and capacity must be positive".to_owned());
    }
    let policy = PlacementPolicy::new(cfg.capacity, cfg.tick_ms * 2)
        .ok_or_else(|| "placement policy rejected the knobs".to_owned())?;
    let scheduler = Scheduler::new(store.clone(), policy);
    let mut rng = Rng(cfg.seed | 1);
    let tag = format!(
        "{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|e| format!("clock: {e}"))?
            .as_nanos()
    );
    let now = |ms: i64| UnixMillis::from_millis(ms).map_err(|e| format!("clock: {e}"));

    // Seed the fleet and the accounts.
    let mut fleet = Vec::new();
    for node in 0..cfg.nodes {
        for i in 0..cfg.workers_per_node {
            let id = WorkerId::parse(format!("wrk-soak-{tag}-n{node}-{i}"))
                .map_err(|e| format!("worker id: {e}"))?;
            store
                .register_worker(&NewWorker {
                    id: &id,
                    tenant: &tenant,
                    name: &format!("soak-node-{node}"),
                    capabilities: &["browser", "mock"],
                    at: now(1_000)?,
                })
                .await
                .map_err(|e| format!("register worker: {e}"))?;
            fleet.push(Fleet {
                id,
                node,
                dead_until: None,
                stale_at: 0,
            });
        }
    }
    let mut accounts = Vec::new();
    for n in 0..cfg.accounts {
        let id = AccountId::parse(format!("acc-soak-{tag}-{n}"))
            .map_err(|e| format!("account id: {e}"))?;
        store
            .upsert_account(
                &Account::new(
                    id.clone(),
                    tenant.clone(),
                    "mock",
                    AccountState::Online,
                    None,
                    now(1_000)?,
                )
                .map_err(|e| format!("account: {e}"))?,
            )
            .await
            .map_err(|e| format!("insert account: {e}"))?;
        accounts.push(id);
    }

    let mut report = cfg.empty_report();
    let mut draining: BTreeSet<WorkerId> = BTreeSet::new();
    let mut undrain_at: Vec<(WorkerId, i64)> = Vec::new();
    let mut generations: BTreeMap<String, i64> = BTreeMap::new();
    let base = 1_700_000_000_000i64;
    let total_ticks = ((cfg.sim_duration_ms + cfg.tick_ms - 1) / cfg.tick_ms).max(1) as u64;
    let started = Instant::now();

    for tick in 0..total_ticks {
        let at = base + (tick as i64) * cfg.tick_ms;

        // Revive dead workers whose recovery instant arrived.
        for member in &mut fleet {
            if let Some(until) = member.dead_until
                && until <= at
            {
                member.dead_until = None;
                report.revives += 1;
            }
        }
        for member in &fleet {
            if member.dead_until.is_none() {
                store
                    .heartbeat_worker(&tenant, &member.id, now(at)?)
                    .await
                    .map_err(|e| format!("heartbeat: {e}"))?;
            }
        }

        // Dead workers past the staleness cutoff lose their leases to
        // the survivors.
        if fleet.iter().any(|m| m.dead_until.is_some()) {
            let takeover = scheduler
                .take_over_lost(&tenant, cfg.tick_ms * 2, cfg.term_ms, now(at)?)
                .await
                .map_err(|e| format!("takeover: {e}"))?;
            fold_migration(&mut report, &takeover.migration);
        }

        // Node drains: the whole node goes away for a few ticks, then
        // comes back.
        if rng.chance(10) {
            let node = rng.below(cfg.nodes as u64) as usize;
            let members: Vec<&Fleet> = fleet.iter().filter(|m| m.node == node).collect();
            let any_alive = members.iter().any(|m| m.dead_until.is_none());
            let all_draining = members.iter().all(|m| draining.contains(&m.id));
            if any_alive && !all_draining {
                let drained = scheduler
                    .drain_node(&tenant, &format!("soak-node-{node}"), cfg.term_ms, now(at)?)
                    .await
                    .map_err(|e| format!("drain node: {e}"))?;
                fold_migration(&mut report, &drained.migration);
                report.node_drains += 1;
                for member in &members {
                    draining.insert(member.id.clone());
                    undrain_at.push((member.id.clone(), at + cfg.tick_ms * 3));
                }
            }
        }
        let due: Vec<(WorkerId, i64)> = undrain_at
            .iter()
            .filter(|(_, when)| *when <= at)
            .cloned()
            .collect();
        for (worker, _) in due {
            store
                .undrain_worker(&tenant, &worker)
                .await
                .map_err(|e| format!("undrain: {e}"))?;
            draining.remove(&worker);
            undrain_at.retain(|(id, _)| id != &worker);
        }

        // Kill a healthy worker now and then: its heartbeats stop, the
        // takeover pass above recovers its leases on a later tick.
        if rng.chance(8) {
            let alive: Vec<usize> = fleet
                .iter()
                .enumerate()
                .filter(|(_, m)| m.dead_until.is_none() && !draining.contains(&m.id))
                .map(|(i, _)| i)
                .collect();
            if alive.len() > 1 {
                let victim = alive[rng.below(alive.len() as u64) as usize];
                fleet[victim].dead_until = Some(at + cfg.tick_ms * 4);
                fleet[victim].stale_at = at + cfg.tick_ms * 2;
                report.worker_deaths += 1;
            }
        }

        // Place everything without a live lease.
        let live = store
            .list_live_leases(&tenant, now(at)?)
            .await
            .map_err(|e| format!("list live leases: {e}"))?;
        let held: BTreeSet<&str> = live.iter().map(|l| l.account_id.as_str()).collect();
        for account in &accounts {
            if held.contains(account.as_str()) {
                continue;
            }
            match scheduler
                .assign(&tenant, account, &[], None, cfg.term_ms, now(at)?)
                .await
            {
                Ok(_) => report.assigns += 1,
                Err(SchedulerError::NoEligibleWorker { .. }) => report.assigns_failed += 1,
                Err(err) => return Err(format!("assign: {err}")),
            }
        }

        // Operation churn on whoever holds a lease.
        if rng.chance(70) {
            let pick = rng.below(live.len().max(1) as u64) as usize;
            if let Some(lease) = live.get(pick) {
                let op = OperationId::parse(format!("op-soak-{tag}-{tick}"))
                    .map_err(|e| format!("operation id: {e}"))?;
                let attempt = AttemptId::parse(format!("att-soak-{tag}-{tick}"))
                    .map_err(|e| format!("attempt id: {e}"))?;
                store
                    .create_operation(&NewOperation {
                        id: &op,
                        tenant: &tenant,
                        kind: "soak_tick",
                        idempotency_key: &format!("key-soak-{tag}-{tick}"),
                        at: now(at)?,
                    })
                    .await
                    .map_err(|e| format!("create operation: {e}"))?;
                let worker = lease.holder.clone();
                store
                    .claim_operation(
                        &tenant,
                        &op,
                        &worker,
                        &attempt,
                        now(at + cfg.tick_ms)?,
                        now(at)?,
                    )
                    .await
                    .map_err(|e| format!("claim: {e}"))?;
                report.operations_claimed += 1;
                let close = match rng.below(10) {
                    0..=5 => AttemptClose::Succeeded,
                    6..=7 => AttemptClose::Failed,
                    _ => AttemptClose::TimedOut,
                };
                store
                    .close_attempt(&tenant, &op, &attempt, &worker, close, now(at)?)
                    .await
                    .map_err(|e| format!("close attempt: {e}"))?;
                report.operations_closed += 1;
            }
        }

        // Safety invariants, checked on a fresh snapshot every tick.
        let live = store
            .list_live_leases(&tenant, now(at)?)
            .await
            .map_err(|e| format!("list live leases: {e}"))?;
        let mut per_holder: BTreeMap<&str, usize> = BTreeMap::new();
        for lease in &live {
            *per_holder.entry(lease.holder.as_str()).or_default() += 1;
            let member = fleet.iter().find(|m| m.id == lease.holder);
            match member {
                None => report.violations.push(format!(
                    "tick {tick}: lease on unknown worker {}",
                    lease.holder
                )),
                Some(member) => {
                    if member.dead_until.is_some() && at >= member.stale_at {
                        report.violations.push(format!(
                            "tick {tick}: live lease on lost worker {} past its staleness cutoff",
                            lease.holder
                        ));
                    }
                    if draining.contains(&lease.holder) {
                        report.violations.push(format!(
                            "tick {tick}: live lease on draining worker {}",
                            lease.holder
                        ));
                    }
                }
            }
            let seen = generations
                .entry(lease.account_id.as_str().to_owned())
                .or_default();
            if lease.generation < *seen {
                report.violations.push(format!(
                    "tick {tick}: generation moved backwards for {} ({} < {})",
                    lease.account_id, lease.generation, seen
                ));
            }
            *seen = lease.generation;
        }
        for (holder, count) in per_holder {
            if count > cfg.capacity {
                report.violations.push(format!(
                    "tick {tick}: worker {holder} holds {count} > capacity {}",
                    cfg.capacity
                ));
            }
        }
        if report.violations.len() >= 20 {
            report.ticks = tick + 1;
            report.sim_elapsed_ms = at - base;
            report.real_elapsed_ms = started.elapsed().as_millis();
            return Ok(report);
        }

        report.ticks = tick + 1;
        report.sim_elapsed_ms = at - base;
        if started.elapsed().as_millis() > cfg.real_cap_ms as u128 {
            report.time_capped = true;
            break;
        }
    }
    report.real_elapsed_ms = started.elapsed().as_millis();
    Ok(report)
}

/// Folds a scheduler migration report into the soak counters.
fn fold_migration(report: &mut SoakReport, migration: &fishpond_scheduler::MigrationReport) {
    report.moved += migration.moved.len() as u64;
    report.contested += migration.contested.len() as u64;
    report.unassigned += migration.unassigned.len() as u64;
}

struct SoakArgs {
    url: Option<String>,
    tenant: Option<String>,
    sim_hours: Option<f64>,
    tick_ms: Option<i64>,
    nodes: Option<usize>,
    workers_per_node: Option<usize>,
    accounts: Option<usize>,
    capacity: Option<usize>,
    seed: Option<u64>,
    real_cap_secs: Option<u64>,
}

const USAGE: &str = "usage: fishpond soak [--database-url <url>] --tenant <id> [--sim-hours <f64>]\n\
                      [--tick-ms <ms>] [--nodes <n>] [--workers-per-node <n>] [--accounts <n>]\n\
                      [--capacity <n>] [--seed <n>] [--real-cap-secs <s>]\n\
                      \n\
                      runs a simulated-horizon stability soak: virtual time advances one tick\n\
                      per iteration (a 72-hour horizon at 30s ticks = 8,640 fast ticks), churn\n\
                      is injected every tick (node drains, worker deaths with takeover,\n\
                      operation claim/close) and safety invariants are re-checked on every\n\
                      tick. Exit 0 clean, 1 on violations/failures, 2 on usage errors. The\n\
                      tenant must exist.";

/// Pulls the flag's value: inline (`--flag=x`) or the next argument.
fn flag_value(
    iter: &mut std::slice::Iter<'_, String>,
    flag: &str,
    inline: Option<String>,
) -> Result<String, ExitCode> {
    if let Some(inline) = inline {
        return Ok(inline);
    }
    match iter.next() {
        Some(value) => Ok(value.clone()),
        None => {
            eprintln!("error: {flag} needs a value\n{USAGE}");
            Err(ExitCode::from(2))
        }
    }
}

fn parse_args(args: &[String]) -> Result<SoakArgs, ExitCode> {
    let mut parsed = SoakArgs {
        url: None,
        tenant: None,
        sim_hours: None,
        tick_ms: None,
        nodes: None,
        workers_per_node: None,
        accounts: None,
        capacity: None,
        seed: None,
        real_cap_secs: None,
    };
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        let (flag, mut inline) = match arg.split_once('=') {
            Some((name, value)) => (name, Some(value.to_owned())),
            None => (arg.as_str(), None),
        };
        let mut next = || flag_value(&mut iter, flag, inline.take());
        let mut number = |flag: &str| -> Result<f64, ExitCode> {
            match next()?.parse::<f64>() {
                Ok(n) if n.is_finite() && n >= 0.0 => Ok(n),
                _ => {
                    eprintln!("error: {flag} expects a non-negative number\n{USAGE}");
                    Err(ExitCode::from(2))
                }
            }
        };
        match flag {
            "--database-url" => parsed.url = Some(next()?),
            "--tenant" => parsed.tenant = Some(next()?),
            "--sim-hours" => parsed.sim_hours = Some(number(flag)?),
            "--tick-ms" => parsed.tick_ms = Some(number(flag)? as i64),
            "--nodes" => parsed.nodes = Some(number(flag)? as usize),
            "--workers-per-node" => parsed.workers_per_node = Some(number(flag)? as usize),
            "--accounts" => parsed.accounts = Some(number(flag)? as usize),
            "--capacity" => parsed.capacity = Some(number(flag)? as usize),
            "--seed" => parsed.seed = Some(number(flag)? as u64),
            "--real-cap-secs" => parsed.real_cap_secs = Some(number(flag)? as u64),
            other => {
                eprintln!("error: unknown argument {other}\n{USAGE}");
                return Err(ExitCode::from(2));
            }
        }
    }
    if parsed.tenant.is_none() {
        eprintln!("error: --tenant <id> is required (the soak does not create tenants)\n{USAGE}");
        return Err(ExitCode::from(2));
    }
    Ok(parsed)
}

/// The `fishpond soak` entry point. Exit 0 clean soak, 1 on violations
/// or failures, 2 on usage errors.
pub fn soak_main(args: &[String]) -> ExitCode {
    let parsed = match parse_args(args) {
        Ok(parsed) => parsed,
        Err(code) => return code,
    };
    let mut cfg = SoakConfig::default();
    if let Some(hours) = parsed.sim_hours {
        cfg.sim_duration_ms = (hours * 3_600_000.0) as i64;
    }
    if let Some(tick) = parsed.tick_ms {
        cfg.tick_ms = tick;
    }
    if let Some(n) = parsed.nodes {
        cfg.nodes = n;
    }
    if let Some(n) = parsed.workers_per_node {
        cfg.workers_per_node = n;
    }
    if let Some(n) = parsed.accounts {
        cfg.accounts = n;
    }
    if let Some(n) = parsed.capacity {
        cfg.capacity = n;
    }
    if let Some(seed) = parsed.seed {
        cfg.seed = seed;
    }
    if let Some(secs) = parsed.real_cap_secs {
        cfg.real_cap_ms = secs * 1_000;
    }
    let (url, _source) = match fishpond_storage::resolve_database_url(parsed.url.as_deref()) {
        Ok(resolved) => resolved,
        Err(e) => {
            eprintln!("soak: {e}");
            return ExitCode::from(1);
        }
    };
    let runtime = match tokio::runtime::Runtime::new() {
        Ok(runtime) => runtime,
        Err(e) => {
            eprintln!("soak: runtime error: {e}");
            return ExitCode::from(1);
        }
    };
    let tenant = parsed.tenant.expect("checked in parse_args");
    let result = runtime.block_on(async move {
        let store = Store::connect(&url)
            .await
            .map_err(|e| format!("connect: {e}"))?;
        run_soak(&store, &tenant, &cfg).await
    });
    match result {
        Ok(report) => {
            let text = render(&report);
            println!("{text}");
            if report.violations.is_empty() && !report.time_capped {
                ExitCode::SUCCESS
            } else {
                ExitCode::from(1)
            }
        }
        Err(e) => {
            eprintln!("soak: failed: {e}");
            ExitCode::from(1)
        }
    }
}
