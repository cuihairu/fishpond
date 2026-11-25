//! fishpond-cli entry point: dispatches operator subcommands.
//!
//! Current surface: `fishpond mcp` (MCP server face, stdio or HTTP).
//! Later milestones add doctor/serve/admin subcommands here.

use std::process::ExitCode;

const USAGE: &str = "usage: fishpond <command> [args]\n\ncommands:\n  mcp   MCP server face (stdio or streamable HTTP)";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("mcp") => fishpond_mcp::cli::mcp_main(&args[1..]),
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
