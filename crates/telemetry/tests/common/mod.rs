//! Helpers shared by the database-backed telemetry tests.
//!
//! Same contract as the other suites' `common` modules: ids are unique
//! per process+nanosecond because primary keys are global and the test
//! database is never cleaned; `gated()` skips when CI has no database.

use std::time::{SystemTime, UNIX_EPOCH};

use fishpond_domain::{TenantId, UnixMillis};
use fishpond_storage::{Store, TenantRecord};

/// Points at the test database; unset means "no database" (the CI path).
pub const ENV: &str = "FISHPOND_TEST_DATABASE_URL";

fn db_url() -> Option<String> {
    std::env::var(ENV).ok().filter(|url| !url.trim().is_empty())
}

/// A token unique to this test invocation so reruns never collide.
pub fn unique_token(tag: &str) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock is after the epoch")
        .as_nanos();
    format!("{tag}-{}-{nanos}", std::process::id())
}

/// A tenant id unique to this test invocation.
pub fn unique_tenant(tag: &str) -> TenantId {
    TenantId::parse(unique_token(tag)).expect("generated tenant id is valid")
}

/// Any other entity id unique to this run (`prefix` keeps it readable).
pub fn unique_entity(prefix: &str, tag: &str) -> String {
    format!("{prefix}-{}", unique_token(tag))
}

/// A connected, migrated store — or `None` when the environment has no
/// database, which is the skip path CI takes.
pub async fn gated() -> Option<Store> {
    let url = db_url()?;
    Some(Store::connect(&url).await.expect("connect + migrate"))
}

/// A non-negative millisecond instant.
pub fn ts(millis: i64) -> UnixMillis {
    UnixMillis::from_millis(millis).expect("test instants are non-negative")
}

/// Real wall-clock milliseconds — deadlines must be relative to this
/// because the overdue probe uses the actual clock.
#[allow(dead_code)] // not every suite in this directory needs it
pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock is after the epoch")
        .as_millis() as i64
}

/// A tenant row for upsert.
pub fn tenant_record(id: &TenantId, name: &str) -> TenantRecord {
    TenantRecord {
        id: id.clone(),
        name: name.to_owned(),
        created_at: ts(1_000),
    }
}
