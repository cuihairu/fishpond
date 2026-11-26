//! F029 migrate: the one-shot migration job (`fishpond migrate`) a
//! compose stack runs before the services come up. Connects, applies
//! pending migrations idempotently, reports the count — secret-free.

use std::process::ExitCode;

use fishpond_storage::{Store, resolve_database_url};

use crate::doctor::parse_url_flag;

/// The `fishpond migrate` entry point. Exit 0 migrated (or nothing to
/// do), 1 on failure, 2 on usage errors.
pub fn migrate_main(args: &[String]) -> ExitCode {
    let flag = match parse_url_flag(args) {
        Ok(flag) => flag,
        Err(code) => return code,
    };

    let (url, _source) = match resolve_database_url(flag.as_deref()) {
        Ok(resolved) => resolved,
        Err(e) => {
            eprintln!("migrate: {e}");
            return ExitCode::from(1);
        }
    };

    let runtime = match tokio::runtime::Runtime::new() {
        Ok(runtime) => runtime,
        Err(e) => {
            eprintln!("migrate: runtime error: {e}");
            return ExitCode::from(1);
        }
    };
    let applied = runtime.block_on(async move {
        let store = Store::connect(&url).await?;
        store.run_migrations().await
    });

    match applied {
        Ok(count) => {
            println!("migrate: connected; {count} migration(s) applied; schema current");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("migrate: failed: {e}");
            ExitCode::from(1)
        }
    }
}
