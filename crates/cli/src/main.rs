//! fishpond-cli entry point: dispatches operator subcommands.
//!
//! Surface: `fishpond mcp` (MCP server face), `fishpond doctor` (F029
//! health checks), `fishpond migrate` (F029 one-shot migration job),
//! `fishpond purge` (F040 retention sweep), `fishpond soak` (F039
//! simulated-stability churn).

use std::process::ExitCode;

const USAGE: &str = "usage: fishpond <command> [args]\n\ncommands:\n  mcp     MCP server face (stdio or streamable HTTP)\n  doctor  read-only deployment health checks\n  migrate apply pending database migrations\n  purge   retention sweep (see: fishpond purge without args)\n  soak    simulated-stability churn (see: fishpond soak without args)";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("mcp") => fishpond_mcp::cli::mcp_main(&args[1..]),
        Some("doctor") => fishpond_cli::doctor::doctor_main(&args[1..]),
        Some("migrate") => fishpond_cli::migrate::migrate_main(&args[1..]),
        Some("purge") => fishpond_cli::purge::purge_main(&args[1..]),
        Some("soak") => fishpond_cli::soak::soak_main(&args[1..]),
        Some(other) => {
            eprintln!("error: unknown command {other}\n{USAGE}");
            ExitCode::from(2)
        }
        None => {
            eprintln!("{USAGE}");
            ExitCode::from(2)
        }
    }
}
