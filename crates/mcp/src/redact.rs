//! Secret redaction for logs, errors and tool output (§安全红线).

/// Renders a redacted, log-safe display form of an API key.
///
/// Keeps at most a short prefix and suffix so operators can correlate
/// log lines with configured keys without exposing the secret.
pub fn key_display(key: &str) -> String {
    let chars: Vec<char> = key.chars().collect();
    let n = chars.len();
    if n == 0 {
        return "<empty>".to_owned();
    }
    if n <= 8 {
        return "*".repeat(n);
    }
    let prefix: String = chars[..3].iter().collect();
    let suffix: String = chars[n - 2..].iter().collect();
    format!("{prefix}…{suffix}")
}

/// Replaces every occurrence of `secret` in `text` with [`key_display`].
///
/// Used as the last line of defense before returning error text that may
/// have absorbed caller input.
pub fn redact_text(text: &str, secret: &str) -> String {
    if secret.is_empty() {
        return text.to_owned();
    }
    text.replace(secret, &key_display(secret))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn masks_short_keys_entirely() {
        assert_eq!(key_display("abc"), "***");
        assert_eq!(key_display("12345678"), "********");
        assert_eq!(key_display(""), "<empty>");
    }

    #[test]
    fn keeps_only_prefix_and_suffix_of_long_keys() {
        let display = key_display("fp_mock_key_default_tenant");
        assert_eq!(display, "fp_…nt");
        assert!(!display.contains("mock_key"));
    }

    #[test]
    fn redacts_secret_occurrences_in_text() {
        let secret = "fp_super_secret_value";
        let cleaned = redact_text("key fp_super_secret_value was rejected", secret);
        assert!(!cleaned.contains(secret));
        assert!(cleaned.contains("fp_…ue"));
    }
}
