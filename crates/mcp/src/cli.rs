//! `fishpond mcp` subcommand entry (stdio mode by default, HTTP optional).
//!
//! M0 bootstrap (replaced by F007/F025): the API key comes from
//! `FISHPOND_API_KEY`; the tenant binding comes from `FISHPOND_TENANT_ID`
//! (default [`DEFAULT_TENANT_ID`], the seeded Mock tenant) and scopes from
//! `FISHPOND_SCOPES` (comma list, default `all`). The backend is always the
//! seeded Mock service bundle; the seeded Mock API keys always resolve too.

use std::process::ExitCode;

use crate::auth::{ApiKeyRecord, ApiKeyStore, ENV_API_KEY, Scope};
use crate::tools::FishpondMcp;

/// Environment variable overriding the tenant bound to the stdio key.
pub const ENV_TENANT_ID: &str = "FISHPOND_TENANT_ID";
/// Environment variable restricting scopes (comma list or `all`).
pub const ENV_SCOPES: &str = "FISHPOND_SCOPES";

/// Tenant the stdio key is bound to when `FISHPOND_TENANT_ID` is unset;
/// it matches the tenant seeded by the Mock service bundle.
pub const DEFAULT_TENANT_ID: &str = "t-default";

const USAGE: &str = "usage: fishpond mcp [--transport stdio|http] [--bind ADDR]";

/// Entry point for `fishpond mcp [args]`.
pub fn mcp_main(args: &[String]) -> ExitCode {
    let mut transport = "stdio".to_owned();
    let mut bind: std::net::SocketAddr = "127.0.0.1:8080".parse().expect("default bind is valid");
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--transport" => match iter.next().map(String::as_str) {
                Some(value @ ("stdio" | "http")) => transport = value.to_owned(),
                _ => {
                    eprintln!("error: --transport expects stdio or http\n{USAGE}");
                    return ExitCode::from(2);
                }
            },
            "--bind" => match iter.next().and_then(|v| v.parse().ok()) {
                Some(value) => bind = value,
                None => {
                    eprintln!("error: --bind expects host:port\n{USAGE}");
                    return ExitCode::from(2);
                }
            },
            "-h" | "--help" => {
                println!("{USAGE}");
                println!();
                println!("  --transport stdio   serve MCP over stdio (default; agent-managed)");
                println!("  --transport http    serve MCP over streamable HTTP at POST /mcp");
                println!("  --bind ADDR         HTTP listen address (default 127.0.0.1:8080)");
                println!();
                println!(
                    "authorization: {ENV_API_KEY} (stdio) or x-fishpond-api-key header (http);"
                );
                println!(
                    "tenant/scopes bootstrap: {ENV_TENANT_ID} (default {DEFAULT_TENANT_ID}), {ENV_SCOPES}"
                );
                return ExitCode::SUCCESS;
            }
            other => {
                eprintln!("error: unknown argument {other}\n{USAGE}");
                return ExitCode::from(2);
            }
        }
    }

    let Some(key) = std::env::var(ENV_API_KEY)
        .ok()
        .filter(|k| !k.trim().is_empty())
    else {
        eprintln!("error: {ENV_API_KEY} is required to start the MCP face");
        return ExitCode::from(2);
    };
    let handler = FishpondMcp::new(
        crate::tools::ServiceBundle::mock(),
        bootstrap_store(),
        Some(key),
    );

    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(e) => {
            eprintln!("error: failed to start tokio runtime: {e}");
            return ExitCode::FAILURE;
        }
    };
    let result = match transport.as_str() {
        "http" => runtime.block_on(crate::server::serve_http(handler, bind)),
        _ => runtime.block_on(crate::server::run_stdio(handler)),
    };
    if let Err(e) = result {
        eprintln!("error: mcp server stopped: {e}");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

/// Builds the M0 key store from environment variables.
///
/// The seeded Mock keys always resolve, so a local run works without
/// configuration; the key from `FISHPOND_API_KEY` is additionally bound to
/// `FISHPOND_TENANT_ID` (default `t-default`) with `FISHPOND_SCOPES`.
fn bootstrap_store() -> ApiKeyStore {
    let key = std::env::var(ENV_API_KEY).unwrap_or_default();
    let tenant = std::env::var(ENV_TENANT_ID)
        .ok()
        .filter(|t| !t.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_TENANT_ID.to_owned());
    let scopes = match std::env::var(ENV_SCOPES) {
        Ok(raw) if !raw.trim().is_empty() && raw.trim() != "all" => raw
            .split(',')
            .filter_map(|name| {
                Scope::ALL
                    .iter()
                    .copied()
                    .find(|s| s.as_str() == name.trim())
            })
            .collect::<Vec<_>>(),
        _ => Scope::ALL.to_vec(),
    };
    let mut store = crate::mock::MockServices::bootstrap_keys();
    if !key.trim().is_empty() {
        store.insert(
            key,
            ApiKeyRecord {
                tenant_id: tenant,
                scopes,
            },
        );
    }
    store
}
