//! F029 doctor: read-only health checks a operator runs before trusting
//! a deployment — url resolution, database reachability, schema
//! currency. Checks never mutate (the store connects unmigrated) and
//! never echo the URL.

use std::process::ExitCode;
use std::time::Instant;

use fishpond_storage::{DbUrlSource, Store, resolve_database_url};

/// One doctor check result. `detail` is secret-free by construction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DoctorCheck {
    /// Stable check name.
    pub name: &'static str,
    /// Whether the check passed.
    pub ok: bool,
    /// Secret-free explanation.
    pub detail: String,
}

/// The full doctor report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DoctorReport {
    /// One entry per check, in execution order.
    pub checks: Vec<DoctorCheck>,
}

impl DoctorReport {
    /// Whether every check passed.
    pub fn ok(&self) -> bool {
        self.checks.iter().all(|check| check.ok)
    }

    /// Human-readable, secret-free rendering (one line per check).
    pub fn render(&self) -> String {
        self.checks
            .iter()
            .map(|check| {
                let mark = if check.ok { "ok" } else { "FAIL" };
                format!("[{mark}] {}: {}", check.name, check.detail)
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// Runs the doctor checks against `store` (connected unmigrated).
pub async fn run_checks(store: &Store, source: DbUrlSource) -> DoctorReport {
    let mut checks = vec![DoctorCheck {
        name: "database_url",
        ok: true,
        detail: source.describe().to_owned(),
    }];

    let started = Instant::now();
    let reachable = match store.ping().await {
        Ok(()) => {
            let ms = started.elapsed().as_millis();
            DoctorCheck {
                name: "database_reachable",
                ok: true,
                detail: format!("select 1 round-tripped in {ms} ms"),
            }
        }
        Err(e) => DoctorCheck {
            name: "database_reachable",
            ok: false,
            detail: format!("select 1 failed: {e}"),
        },
    };
    checks.push(reachable);

    checks.push(match store.schema_current().await {
        Ok(true) => DoctorCheck {
            name: "schema_current",
            ok: true,
            detail: "every embedded migration is applied".to_owned(),
        },
        Ok(false) => DoctorCheck {
            name: "schema_current",
            ok: false,
            detail: "pending migrations exist; run `fishpond migrate`".to_owned(),
        },
        Err(e) => DoctorCheck {
            name: "schema_current",
            ok: false,
            detail: format!("migration table unreadable: {e}"),
        },
    });

    DoctorReport { checks }
}

/// Entry point of `fishpond doctor`. Exit 0 healthy, 1 unhealthy, 2 on
/// usage errors.
pub fn doctor_main(args: &[String]) -> ExitCode {
    let flag = match parse_url_flag(args) {
        Ok(flag) => flag,
        Err(code) => return code,
    };

    let (url, source) = match resolve_database_url(flag.as_deref()) {
        Ok(resolved) => resolved,
        Err(e) => {
            eprintln!("[FAIL] database_url: {e}");
            return ExitCode::from(1);
        }
    };

    let runtime = match tokio::runtime::Runtime::new() {
        Ok(runtime) => runtime,
        Err(e) => {
            eprintln!("[FAIL] tokio runtime: {e}");
            return ExitCode::from(1);
        }
    };
    let report = runtime.block_on(async move {
        match Store::connect_unmigrated(&url).await {
            Ok(store) => run_checks(&store, source).await,
            Err(e) => DoctorReport {
                checks: vec![
                    DoctorCheck {
                        name: "database_url",
                        ok: true,
                        detail: source.describe().to_owned(),
                    },
                    DoctorCheck {
                        name: "database_reachable",
                        ok: false,
                        detail: format!("connect failed: {e}"),
                    },
                ],
            },
        }
    });

    println!("fishpond doctor");
    println!("{}", report.render());
    if report.ok() {
        println!("doctor: healthy ({} checks passed)", report.checks.len());
        ExitCode::SUCCESS
    } else {
        println!("doctor: unhealthy");
        ExitCode::from(1)
    }
}

/// Extracts `--database-url <url>` (or `--database-url=<url>`); a usage
/// problem returns the process exit code to use.
pub fn parse_url_flag(args: &[String]) -> Result<Option<String>, ExitCode> {
    let mut flag = None;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if arg == "--database-url" {
            match iter.next() {
                Some(value) => flag = Some(value.clone()),
                None => {
                    eprintln!("error: --database-url needs a value\n");
                    return Err(ExitCode::from(2));
                }
            }
        } else if let Some(value) = arg.strip_prefix("--database-url=") {
            flag = Some(value.to_owned());
        } else {
            eprintln!("error: unknown argument {arg}\n");
            return Err(ExitCode::from(2));
        }
    }
    Ok(flag)
}
