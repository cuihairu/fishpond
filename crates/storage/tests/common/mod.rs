//! Helpers shared by the database-backed tests.
//!
//! Each `tests/*.rs` file is its own crate, so anything two of them need
//! belongs here. Every id helper stamps the current process and a
//! nanosecond clock, because primary keys are global and the test database
//! is deliberately never cleaned — reruns must not collide.

use std::time::{SystemTime, UNIX_EPOCH};

use fishpond_domain::{Account, AccountId, AccountState, TenantId, UnixMillis};
use fishpond_storage::{Store, TenantRecord};

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

/// A tenant id unique to this test invocation so reruns never collide.
pub fn unique_tenant(tag: &str) -> TenantId {
    TenantId::parse(unique_token(tag)).expect("generated tenant id is valid")
}

/// Any other entity id unique to this run (`prefix` keeps it readable).
pub fn unique_entity(prefix: &str, tag: &str) -> String {
    format!("{prefix}-{}", unique_token(tag))
}

/// A master-key generation unique to this run — `master_keys.version` is
/// a global integer primary key and envelopes record it, so test runs
/// stamp distinct positive generations.
// Only the credential tests mint key generations; the other binaries
// that include this module never do (each binary compiles it separately).
#[allow(dead_code)]
pub fn unique_version() -> i32 {
    let micros = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock is after the epoch")
        .subsec_micros() as i32;
    (std::process::id() as i32 % 2_000) * 1_000_000 + micros % 1_000_000
}

/// A connected, migrated store — or `None` when the environment has no
/// database, which is the skip path CI takes (§7.3: Mock is the only CI
/// target).
pub async fn gated() -> Option<Store> {
    let url = db_url()?;
    Some(Store::connect(&url).await.expect("connect + migrate"))
}

/// Builds a non-negative millisecond instant.
pub fn ts(millis: i64) -> UnixMillis {
    UnixMillis::from_millis(millis).expect("non-negative millis")
}

/// A tenant row with the given id and name.
pub fn tenant_record(id: &TenantId, name: &str) -> TenantRecord {
    TenantRecord {
        id: id.clone(),
        name: name.to_owned(),
        created_at: ts(1_000),
    }
}

/// A valid account owned by `tenant` (ONLINE, no note).
// Only the repository tests build an account; the other binaries that
// include this module never do, and each binary compiles the module
// separately — hence the explicit allowance.
#[allow(dead_code)]
pub fn account(id: &str, tenant: &TenantId, state: AccountState, note: Option<String>) -> Account {
    Account::new(
        AccountId::parse(id).expect("account id is valid"),
        tenant.clone(),
        "mock",
        state,
        note,
        ts(2_000),
    )
    .expect("account satisfies its invariants")
}
