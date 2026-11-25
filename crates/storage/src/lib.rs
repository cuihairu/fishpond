//! fishpond-storage: persistence — PostgreSQL migrations and tenant-scoped
//! repositories (§3.3).
//!
//! Queries are tenant-scoped by construction; F007 layers row-level
//! security on top. CI builds without a database: migrations embed at
//! compile time and the tests skip when `FISHPOND_TEST_DATABASE_URL` is
//! unset.

pub mod error;
pub mod store;

pub use error::{StorageError, StorageResult};
pub use store::{Store, TenantRecord};
