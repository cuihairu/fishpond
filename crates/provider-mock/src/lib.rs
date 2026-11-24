//! fishpond-provider-mock: Mock platform and provider: the default and only CI target (§7.3).

/// Reports the scaffold status of the mock provider.
///
/// Placeholder for F005, which replaces this with the real Mock Platform
/// surface (authorization, writes, disconnects, duplicate-event fault switches).
pub fn scaffold_status() -> &'static str {
    "placeholder"
}

#[cfg(test)]
mod tests {
    #[test]
    fn skeleton_compiles_and_links() {
        assert_eq!(super::scaffold_status(), "placeholder");
    }
}
