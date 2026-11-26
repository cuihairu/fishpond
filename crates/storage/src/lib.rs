//! fishpond-storage: persistence — PostgreSQL migrations and tenant-scoped
//! repositories (§3.3).
//!
//! Tenant isolation is enforced twice: every query filters on `tenant_id`,
//! and migration 0002 adds row-level security that applies to the
//! application role as well (`FORCE ROW LEVEL SECURITY`). Each repository
//! call therefore runs in a transaction bound to its tenant, and unbound
//! sessions fail closed.
//!
//! `authz` (F007) holds API keys as SHA-256 hashes with frozen RBAC scopes;
//! CI builds without a database: migrations embed at compile time and the
//! tests skip when `FISHPOND_TEST_DATABASE_URL` is unset.

pub mod authz;
pub mod credentials;
pub mod error;
pub mod leases;
pub mod observations;
pub mod operations;
pub mod sessions;
pub mod store;
pub mod workers;

pub use authz::{
    ApiKeyRecord, ResolvedKey, RoleRecord, SCOPE_ACCOUNTS_READ, SCOPE_ALL, SCOPE_CATALOG_READ,
    SCOPE_CONVERSATIONS_READ, SCOPE_CONVERSATIONS_WRITE, SCOPE_EVENTS_READ, SCOPE_OPERATIONS_READ,
    hash_api_key, require_scope,
};
pub use credentials::{
    CredentialCipher, CredentialRecord, MasterKey, NewCredential, SealedSecret, SecretBox,
};
pub use error::{StorageError, StorageResult};
pub use leases::AccountLease;
pub use observations::{NewObservation, ObservationOutcome, ObservationRecord, ObservationSource};
pub use operations::{
    AttemptClose, AttemptRecord, ClaimedOperation, CreatedOperation, ExpiredAttempt, NewOperation,
};
pub use sessions::{AuthSessionRecord, AuthSessionUpdate, NewAuthSession};
pub use store::{Store, TenantRecord};
pub use workers::{NewWorker, WorkerRecord};
