//! Tracing init (F030 tracing): one place that installs the
//! `tracing-subscriber` fmt layer with an `RUST_LOG`-style env filter.
//! Idempotent — a second init keeps the first subscriber (useful when
//! both a library test and a binary call it). No spans are emitted by
//! this module itself; the collectors log at debug level.

use std::sync::atomic::{AtomicBool, Ordering};

static INITIALIZED: AtomicBool = AtomicBool::new(false);

/// Installs the subscriber once. `default_filter` is the filter used
/// when `RUST_LOG` is unset (e.g. `"info"`). Errors from a second init
/// are swallowed by design — the first subscriber wins.
pub fn init_tracing(default_filter: &str) {
    if INITIALIZED.swap(true, Ordering::SeqCst) {
        return;
    }
    let filter = std::env::var("RUST_LOG").unwrap_or_else(|_| default_filter.to_owned());
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::new(filter))
        .with_target(false)
        .try_init();
}

#[cfg(test)]
mod tests {
    #[test]
    fn double_init_does_not_panic() {
        super::init_tracing("info");
        super::init_tracing("debug");
    }
}
