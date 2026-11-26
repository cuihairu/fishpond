//! fishpond-cli: Operator CLI: doctor, migration and administration
//! commands. Every report is secret-free — the database URL is
//! described by source, never by value.

pub mod doctor;
pub mod migrate;
