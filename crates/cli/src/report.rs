//! F039 capacity report: the operational face (`fishpond report`).
//! Connects, enumerates the tenants visible to the connection's role
//! (an RLS-restricted role sees none and must pass `--tenant`
//! explicitly — the CLI fails closed rather than reporting an empty
//! fleet as success), and reads each tenant's capacity picture through
//! its bound transaction. Every line is secret-free.

use std::process::ExitCode;

use fishpond_domain::TenantId;
use fishpond_storage::{CapacityReport, Store, resolve_database_url};

/// One tenant's line of the report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TenantReport {
    /// The tenant id.
    pub tenant: String,
    /// Its capacity picture.
    pub report: CapacityReport,
}

/// The secret-free multi-line report (one line per tenant).
pub fn render(reports: &[TenantReport]) -> String {
    reports
        .iter()
        .map(|line| {
            format!(
                "tenant={} accounts={} online={} workers={} draining={} \
                 live_leases={} ops_pending={} ops_running={} ops_terminal={} \
                 events={} profiles={} sessions={} webhooks={}",
                line.tenant,
                line.report.accounts_total,
                line.report.accounts_online,
                line.report.workers_total,
                line.report.workers_draining,
                line.report.live_leases,
                line.report.operations_pending,
                line.report.operations_running,
                line.report.operations_terminal,
                line.report.events_total,
                line.report.profiles_active,
                line.report.sessions_active,
                line.report.webhook_subscriptions,
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Connects and reports every requested tenant (`--all` passes an
/// empty list: every tenant the role can see). An empty explicit list
/// with zero visible tenants is an error — an RLS-bound role must not
/// silently report success over a deployment it cannot see.
pub async fn run_report(url: &str, tenants: &[String]) -> Result<Vec<TenantReport>, String> {
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
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|e| format!("clock: {e}"))?
        .as_millis() as i64;
    let now = fishpond_domain::UnixMillis::from_millis(now_ms).map_err(|e| e.to_string())?;
    let mut reports = Vec::with_capacity(ids.len());
    for raw in ids {
        let tenant = TenantId::parse(raw.clone()).map_err(|e| format!("tenant {raw}: {e}"))?;
        let report = store
            .tenant_capacity(&tenant, now)
            .await
            .map_err(|e| format!("tenant {raw}: {e}"))?;
        reports.push(TenantReport {
            tenant: raw,
            report,
        });
    }
    Ok(reports)
}

struct ReportArgs {
    url: Option<String>,
    tenants: Vec<String>,
}

const USAGE: &str = "usage: fishpond report [--database-url <url>] (--tenant <id>... | --all)\n\n\
                      per-tenant capacity picture: accounts, workers (draining), live leases, \
                      operations by state, events, profiles, sessions, webhooks";

fn parse_args(args: &[String]) -> Result<ReportArgs, ExitCode> {
    let mut parsed = ReportArgs {
        url: None,
        tenants: Vec::new(),
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
            "--all" => all = true,
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
    if all {
        parsed.tenants.clear();
    }
    Ok(parsed)
}

/// The `fishpond report` entry point. Exit 0 reported, 1 on failure,
/// 2 on usage errors.
pub fn report_main(args: &[String]) -> ExitCode {
    let parsed = match parse_args(args) {
        Ok(parsed) => parsed,
        Err(code) => return code,
    };
    let (url, _source) = match resolve_database_url(parsed.url.as_deref()) {
        Ok(resolved) => resolved,
        Err(e) => {
            eprintln!("report: {e}");
            return ExitCode::from(1);
        }
    };
    let runtime = match tokio::runtime::Runtime::new() {
        Ok(runtime) => runtime,
        Err(e) => {
            eprintln!("report: runtime error: {e}");
            return ExitCode::from(1);
        }
    };
    match runtime.block_on(run_report(&url, &parsed.tenants)) {
        Ok(reports) => {
            println!("{}", render(&reports));
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("report: failed: {e}");
            ExitCode::from(1)
        }
    }
}
