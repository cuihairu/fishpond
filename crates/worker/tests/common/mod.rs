//! Helpers shared by the database-backed worker tests.
//!
//! Same contract as `fishpond-storage/tests/common`: ids are unique per
//! process+nanosecond because primary keys are global and the test
//! database is never cleaned; `gated()` skips when CI has no database.

use std::time::{SystemTime, UNIX_EPOCH};

use fishpond_domain::{Account, AccountId, AccountState, TenantId, UnixMillis};
use fishpond_storage::{Store, TenantRecord};

/// Points at the test database; unset means "no database" (the CI path).
pub const ENV: &str = "FISHPOND_TEST_DATABASE_URL";

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

/// Any other entity id unique to this run.
pub fn unique_entity(prefix: &str, tag: &str) -> String {
    format!("{prefix}-{}", unique_token(tag))
}

/// A connected, migrated store — or `None` when the environment has no
/// database, which is the skip path CI takes.
pub async fn gated() -> Option<Store> {
    let url = std::env::var(ENV).ok().filter(|v| !v.trim().is_empty())?;
    Some(Store::connect(&url).await.expect("connect + migrate"))
}

/// Builds a non-negative millisecond instant.
pub fn ts(millis: i64) -> UnixMillis {
    UnixMillis::from_millis(millis).expect("non-negative millis")
}

/// A tenant row for upsert.
pub fn tenant_record(id: &TenantId, name: &str) -> TenantRecord {
    TenantRecord {
        id: id.clone(),
        name: name.to_owned(),
        created_at: ts(1_000),
    }
}

/// A valid account owned by `tenant` (ONLINE, no note).
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
