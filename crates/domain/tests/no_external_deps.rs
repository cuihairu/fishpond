//! Pins the §3.3 layering rule: `fishpond-domain` must stay dependency-free.
//!
//! Any dependency added to `fishpond-domain` (including dev/build dependencies)
//! is a contract change and must be an explicit, reviewed decision — update
//! this test only as part of such a decision.

/// No `[dependencies]`, `[dev-dependencies]` or `[build-dependencies]` table
/// may appear in the domain manifest.
#[test]
fn domain_has_no_external_dependencies() {
    let manifest = include_str!("../Cargo.toml");
    for forbidden in [
        "[dependencies]",
        "[dev-dependencies]",
        "[build-dependencies]",
    ] {
        assert!(
            !manifest.contains(forbidden),
            "fishpond-domain must stay dependency-free (§3.3): found `{forbidden}` in crates/domain/Cargo.toml"
        );
    }
}
