//! Secret redaction: API keys must never appear in logs or error text
//! (§安全红线), while still being correlatable via their redacted display.

mod common;

use std::io::Write;
use std::sync::{Arc, Mutex};

use common::{call_error, call_json, spawn_mock};
use fishpond_mcp::mock::KEY_DEFAULT_TENANT;
use fishpond_mcp::redact::key_display;
use serde_json::json;
use tracing_subscriber::util::SubscriberInitExt;

#[derive(Clone)]
struct SharedBuf(Arc<Mutex<Vec<u8>>>);

impl Write for SharedBuf {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().expect("log buffer").extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for SharedBuf {
    type Writer = SharedBuf;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

/// Captures tracing output of one test and asserts the raw API key never
/// appears, on both the rejected-key and accepted-key paths.
#[tokio::test(flavor = "current_thread")]
async fn api_key_never_appears_in_logs_or_errors() {
    let buf = SharedBuf(Arc::new(Mutex::new(Vec::new())));
    let subscriber = tracing_subscriber::fmt()
        .with_writer(buf.clone())
        .with_ansi(false)
        .with_max_level(tracing::Level::DEBUG)
        .finish();
    let _guard = subscriber.set_default();

    // Rejected-key path logs a warning with the redacted display form.
    let raw_bad = "fp_secret_key_do_not_log_123456";
    let client = spawn_mock(Some(raw_bad.to_owned())).await;
    let err = call_error(&client, "list_accounts", json!({})).await;
    assert!(err.to_lowercase().contains("unauthenticated"));
    assert!(!err.contains(raw_bad), "error leaked the API key: {err}");

    // Accepted-key path logs a debug line with the redacted display form.
    let good = spawn_mock(Some(KEY_DEFAULT_TENANT.to_owned())).await;
    let out = call_json(&good, "list_accounts", json!({})).await;
    assert_eq!(out["items"].as_array().unwrap().len(), 3);

    let logged = String::from_utf8(buf.0.lock().expect("log buffer").clone()).expect("utf8 logs");
    assert!(
        !logged.contains(raw_bad),
        "logs leaked the API key:\n{logged}"
    );
    assert!(
        !logged.contains(KEY_DEFAULT_TENANT),
        "logs leaked the mock API key:\n{logged}"
    );
    assert!(
        logged.contains(&key_display(raw_bad)),
        "logs should contain the redacted display form:\n{logged}"
    );
    assert!(
        logged.contains(&key_display(KEY_DEFAULT_TENANT)),
        "logs should contain the redacted display of the accepted key:\n{logged}"
    );
}
