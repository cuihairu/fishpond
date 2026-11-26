//! F040 purge: the operational retention sweep (`fishpond purge`).
//! Connects, enumerates the tenants visible to the connection's role
//! (an RLS-restricted role sees none and must pass `--tenant`
//! explicitly — the CLI fails closed rather than sweeping nothing and
//! reporting success), and purges each tenant through its bound
//! transaction. `--dry-run` rolls the sweep back and reports what a
//! real pass would remove. Every report is secret-free.

use std::process::ExitCode;

use fishpond_domain::{TenantId, UnixMillis};
use fishpond_storage::{PurgeReport, Store, resolve_database_url};

/// Aggregated sweep result across the tenants processed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PurgeSummary {
    /// Tenants swept.
    pub tenants: usize,
    /// Sessions removed (or a dry run would remove).
    pub sessions: u64,
    /// Takeover grants removed.
    pub grants: u64,
    /// Operation attempts removed.
    pub attempts: u64,
    /// Operations removed.
    pub operations: u64,
    /// Webhook deliveries removed.
    pub deliveries: u64,
    /// `true` when the sweep was rolled back (dry run).
    pub dry_run: bool,
}

impl PurgeSummary {
    fn add(&mut self, report: &PurgeReport) {
        self.sessions += report.sessions;
        self.grants += report.grants;
        self.attempts += report.attempts;
        self.operations += report.operations;
        self.deliveries += report.deliveries;
    }
}

/// The secret-free one-line report.
pub fn render(summary: &PurgeSummary) -> String {
    format!(
        "purge{}: tenants={} sessions={} grants={} attempts={} operations={} deliveries={}",
        if summary.dry_run {
            " (dry run — rolled back)"
        } else {
            ""
        },
        summary.tenants,
        summary.sessions,
        summary.grants,
        summary.attempts,
        summary.operations,
        summary.deliveries,
    )
}

/// Connects, resolves the tenant list (explicit ids, or every tenant
/// the role can see when the list is empty), and sweeps. An empty
/// explicit list with zero visible tenants is an error — an RLS-bound
/// role must not silently report success over a deployment it cannot
/// see.
pub async fn run_purge(
    url: &str,
    before: UnixMillis,
    dry_run: bool,
    tenants: &[String],
) -> Result<PurgeSummary, String> {
    let store = Store::connect(url)
        .await
        .map_err(|e| format!("connect: {e}"))?;
    let ids = if tenants.is_empty() {
        let visible = store
            .visible_tenant_ids()
            .await
            .map_err(|e| format!("enumerate tenants: {e}"))?;
        if visible.is_empty() {
            return Err(
                "no tenants visible to this role (row-level security fails closed); \
                 pass --tenant explicitly"
                    .to_owned(),
            );
        }
        visible
    } else {
        tenants.to_vec()
    };
    let mut summary = PurgeSummary {
        tenants: ids.len(),
        dry_run,
        ..PurgeSummary::default()
    };
    for raw in ids {
        let tenant = TenantId::parse(raw.clone()).map_err(|e| format!("tenant {raw}: {e}"))?;
        let report = if dry_run {
            store
                .dry_run_purge(&tenant, before)
                .await
                .map_err(|e| format!("tenant {raw}: {e}"))?
        } else {
            store
                .purge_expired(&tenant, before)
                .await
                .map_err(|e| format!("tenant {raw}: {e}"))?
        };
        summary.add(&report);
    }
    Ok(summary)
}

struct PurgeArgs {
    url: Option<String>,
    tenants: Vec<String>,
    before: Option<i64>,
    dry_run: bool,
}

const USAGE: &str = "usage: fishpond purge [--database-url <url>] (--tenant <id>... | --all) \
                      --before <unix-ms> [--dry-run]\n\nremoves terminal sessions, grants, \
                      attempts, operations and deliveries closed before the cutoff; \
                      the event log is never swept";

fn parse_args(args: &[String]) -> Result<PurgeArgs, ExitCode> {
    let mut parsed = PurgeArgs {
        url: None,
        tenants: Vec::new(),
        before: None,
        dry_run: false,
    };
    let mut all = false;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        let (flag, inline) = match arg.split_once('=') {
            Some((name, value)) => (name, Some(value.to_owned())),
            None => (arg.as_str(), None),
        };
        let value = || -> Result<String, ExitCode> {
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
        };
        match flag {
            "--database-url" => parsed.url = Some(value()?),
            "--tenant" => parsed.tenants.push(value()?),
            "--before" => match value()?.parse::<i64>() {
                Ok(millis) if millis >= 0 => parsed.before = Some(millis),
                _ => {
                    eprintln!("error: --before expects a unix-ms instant\n{USAGE}");
                    return Err(ExitCode::from(2));
                }
            },
            "--all" => all = true,
            "--dry-run" => parsed.dry_run = true,
            other => {
                eprintln!("error: unknown argument {other}\n{USAGE}");
                return Err(ExitCode::from(2));
            }
        }
    }
    if !all && parsed.tenants.is_empty() {
        eprintln!("error: pass --tenant <id> (repeatable) or --all\n{USAGE}");
        return Err(ExitCode::from(2));
    }
    if parsed.before.is_none() {
        eprintln!(
            "error: --before <unix-ms> is required (explicit cutoffs prevent \
                   sweeping fresh rows by accident)\n{USAGE}"
        );
        return Err(ExitCode::from(2));
    }
    if all {
        parsed.tenants.clear();
    }
    Ok(parsed)
}

/// The `fishpond purge` entry point. Exit 0 swept (or nothing to do),
/// 1 on failure, 2 on usage errors.
pub fn purge_main(args: &[String]) -> ExitCode {
    let parsed = match parse_args(args) {
        Ok(parsed) => parsed,
        Err(code) => return code,
    };
    let (url, _source) = match resolve_database_url(parsed.url.as_deref()) {
        Ok(resolved) => resolved,
        Err(e) => {
            eprintln!("purge: {e}");
            return ExitCode::from(1);
        }
    };
    let runtime = match tokio::runtime::Runtime::new() {
        Ok(runtime) => runtime,
        Err(e) => {
            eprintln!("purge: runtime error: {e}");
            return ExitCode::from(1);
        }
    };
    let result = runtime.block_on(async move {
        run_purge(
            &url,
            UnixMillis::from_millis(parsed.before.unwrap_or(0)).map_err(|e| e.to_string())?,
            parsed.dry_run,
            &parsed.tenants,
        )
        .await
    });
    match result {
        Ok(summary) => {
            println!("{}", render(&summary));
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("purge: failed: {e}");
            ExitCode::from(1)
        }
    }
}
