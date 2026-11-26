//! Helpers shared by the monitor tests (same pattern as storage's).

use std::time::{SystemTime, UNIX_EPOCH};

use fishpond_storage::Store;

/// Points at the test database; unset means "no database" (the CI path).
pub const ENV: &str = "FISHPOND_TEST_DATABASE_URL";

/// The configured database URL, if any.
pub fn db_url() -> Option<String> {
    std::env::var(ENV).ok().filter(|v| !v.trim().is_empty())
}

/// A token unique to this test invocation so reruns never collide.
pub fn unique_token(tag: &str) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock is after the epoch")
        .as_nanos();
    format!("{tag}-{}-{nanos}", std::process::id())
}

/// A connected, migrated store — or `None` when the environment has no
/// database, which is the skip path CI takes (§7.3: Mock is the only CI
/// target).
pub async fn gated() -> Option<Store> {
    let url = db_url()?;
    Some(Store::connect(&url).await.expect("connect + migrate"))
}
