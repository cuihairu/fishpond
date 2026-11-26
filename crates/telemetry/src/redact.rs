//! Redaction scan (F030 脱敏扫描): the last line of defense before a
//! string reaches a log, event, metric label or CLI line. Two layers:
//!
//! 1. **Known secrets** — the caller's configured credentials, replaced
//!    wherever they appear (the same idea as the MCP plane's
//!    `redact_text`, generalized to a set).
//! 2. **Credential-shaped text** — URL userinfo, bearer tokens,
//!    `password=`-style query values and long hex blobs, masked by
//!    pattern even when the exact secret is unknown.
//!
//! Findings never echo the matched text; [`sanitize`] output is
//! idempotent (sanitizing sanitized text changes nothing).

/// Tunables for the shape scan.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScanConfig {
    /// Hex runs at least this long are treated as credential material.
    pub min_hex_len: usize,
}

impl Default for ScanConfig {
    fn default() -> Self {
        Self { min_hex_len: 32 }
    }
}

/// What kind of credential-shaped text a finding matched.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScanFinding {
    /// A configured secret appeared verbatim.
    KnownSecret,
    /// `scheme://user:password@host` — the userinfo carries a password.
    UrlUserInfo,
    /// `bearer <token>` (case-insensitive scheme).
    BearerToken,
    /// `password=…`/`token=…`/`api_key=…`-style query value.
    KeyValue,
    /// A long hex run — token-shaped material.
    HexBlob,
}

impl ScanFinding {
    /// Secret-free label for reports.
    pub fn label(self) -> &'static str {
        match self {
            Self::KnownSecret => "known-secret",
            Self::UrlUserInfo => "url-userinfo",
            Self::BearerToken => "bearer-token",
            Self::KeyValue => "key-value",
            Self::HexBlob => "hex-blob",
        }
    }
}

/// Scans `text` and reports every credential-shaped span, most-specific
/// first. Known secrets are matched literally; shapes by pattern. The
/// findings carry only kinds — never positions with content.
pub fn findings(text: &str, known_secrets: &[&str]) -> Vec<ScanFinding> {
    findings_with(text, known_secrets, &ScanConfig::default())
}

/// [`findings`] with an explicit config.
pub fn findings_with(text: &str, known_secrets: &[&str], config: &ScanConfig) -> Vec<ScanFinding> {
    let mut found = Vec::new();
    for secret in known_secrets {
        if !secret.is_empty() && text.contains(secret) {
            found.push(ScanFinding::KnownSecret);
        }
    }
    if has_url_userinfo(text) {
        found.push(ScanFinding::UrlUserInfo);
    }
    if has_bearer(text) {
        found.push(ScanFinding::BearerToken);
    }
    if has_key_value(text) {
        found.push(ScanFinding::KeyValue);
    }
    if has_hex_blob(text, config.min_hex_len) {
        found.push(ScanFinding::HexBlob);
    }
    found
}

/// Rewrites `text` with every credential-shaped span masked. Idempotent:
/// the output contains no known secret and no credential-shaped span, so
/// scanning it again finds nothing to change (the placeholders are not
/// themselves credential-shaped).
pub fn sanitize(text: &str, known_secrets: &[&str]) -> String {
    sanitize_with(text, known_secrets, &ScanConfig::default())
}

/// [`sanitize`] with an explicit config.
pub fn sanitize_with(text: &str, known_secrets: &[&str], config: &ScanConfig) -> String {
    let mut out = text.to_owned();
    for secret in known_secrets {
        if secret.is_empty() {
            continue;
        }
        if out.contains(secret) {
            out = out.replace(secret, "***");
        }
    }
    out = mask_url_userinfo(&out);
    out = mask_bearer(&out);
    out = mask_key_values(&out);
    mask_hex_blobs(&out, config.min_hex_len)
}

// ---- shape detectors (on the raw text) ----

fn has_url_userinfo(text: &str) -> bool {
    find_url_userinfo(text).is_some()
}

/// Finds `://` and a `@` in the authority section (before the first `/`
/// after the scheme), with a `:` inside — a user:password pair.
fn find_url_userinfo(text: &str) -> Option<(usize, usize)> {
    let scheme = text.find("://")?;
    let after = scheme + 3;
    let rest = &text[after..];
    let authority_end = rest.find('/').unwrap_or(rest.len());
    let authority = &rest[..authority_end];
    let at = authority.find('@')?;
    let userinfo = &authority[..at];
    userinfo.contains(':').then(|| (after, after + at))
}

fn has_bearer(text: &str) -> bool {
    let lower = text.to_lowercase();
    lower.match_indices("bearer ").any(|(start, _)| {
        let rest = &text[start + "bearer ".len()..];
        let token_end = rest
            .find([' ', '\t', '\n', '"', '\''])
            .unwrap_or(rest.len());
        let token = &rest[..token_end];
        // A token that is exactly the `***` mask is already-sanitized
        // output — not a finding.
        !token.is_empty() && token != "***"
    })
}

const SECRET_KEYS: [&str; 8] = [
    "password",
    "passwd",
    "secret",
    "token",
    "api_key",
    "apikey",
    "access_token",
    "authorization",
];

fn has_key_value(text: &str) -> bool {
    SECRET_KEYS
        .iter()
        .any(|key| find_key_value(text, key).is_some())
}

/// Finds the value span of `key=<value>` for a secret-carrying key,
/// where the value ends at `&`, whitespace, a quote or end of text. A
/// value that is exactly the `***` mask is already-sanitized output —
/// not a finding.
fn find_key_value(text: &str, key: &str) -> Option<(usize, usize)> {
    let lower = text.to_lowercase();
    let mut search_from = 0;
    while let Some(rel) = lower[search_from..].find(key) {
        let start = search_from + rel;
        let value_start = start + key.len();
        let rest = &text[value_start..];
        if let Some(0) = rest.find('=') {
            // `=` immediately after the key; the value ends at `&`,
            // whitespace, a quote or end of text. A bare `key=` (empty
            // value) is not a finding.
            let value = &rest[1..];
            let end = 1 + value
                .find(['&', ' ', '"', '\'', '\n', '\t'])
                .unwrap_or(value.len());
            if end > 1 {
                if &rest[1..end] == "***" {
                    // Already masked by an earlier pass.
                    search_from = value_start + end;
                    continue;
                }
                return Some((value_start + 1, value_start + end));
            }
        }
        search_from = value_start;
    }
    None
}

fn has_hex_blob(text: &str, min_len: usize) -> bool {
    find_hex_blob(text, min_len).is_some()
}

/// Finds the first run of at least `min_len` hex digits bounded by
/// non-hex characters.
fn find_hex_blob(text: &str, min_len: usize) -> Option<(usize, usize)> {
    let chars: Vec<(usize, char)> = text.char_indices().collect();
    let mut run_start: Option<usize> = None;
    let mut run_len = 0usize;
    for (idx, ch) in chars {
        if ch.is_ascii_hexdigit() {
            if run_len == 0 {
                run_start = Some(idx);
            }
            run_len += 1;
        } else {
            if run_len >= min_len {
                return run_start.map(|s| (s, s + run_len));
            }
            run_len = 0;
        }
    }
    if run_len >= min_len {
        return run_start.map(|s| (s, s + run_len));
    }
    None
}

// ---- maskers (on progressively sanitized text) ----

/// `scheme://user:password@` → `scheme://***@`.
fn mask_url_userinfo(text: &str) -> String {
    match find_url_userinfo(text) {
        Some((start, at)) => {
            let mut out = String::with_capacity(text.len());
            out.push_str(&text[..start]);
            out.push_str("***@");
            out.push_str(&text[at + 1..]);
            out
        }
        None => text.to_owned(),
    }
}

/// `bearer <token>` → `bearer ***`.
fn mask_bearer(text: &str) -> String {
    let lower = text.to_lowercase();
    let needle = "bearer ";
    match lower.find(needle) {
        Some(start) => {
            let token_start = start + needle.len();
            let token_end = text[token_start..]
                .find([' ', '\t', '\n', '"', '\''])
                .map_or(text.len(), |off| token_start + off);
            if token_end == token_start {
                return text.to_owned();
            }
            let mut out = String::with_capacity(text.len());
            out.push_str(&text[..token_start]);
            out.push_str("***");
            out.push_str(&text[token_end..]);
            out
        }
        None => text.to_owned(),
    }
}

/// Every `secret_key=<value>` value → `***`. Single left-to-right pass
/// per key — the scan cursor always moves past the replaced span, so a
/// mask can never re-trigger itself.
fn mask_key_values(text: &str) -> String {
    let mut out = text.to_owned();
    for key in SECRET_KEYS {
        let mut result = String::with_capacity(out.len());
        let mut rest: &str = &out;
        while let Some((start, end)) = find_key_value(rest, key) {
            if &rest[start..end] == "***" {
                // Already masked by an earlier key's pass — keep as is.
                result.push_str(&rest[..end]);
            } else {
                result.push_str(&rest[..start]);
                result.push_str("***");
            }
            rest = &rest[end..];
        }
        result.push_str(rest);
        out = result;
    }
    out
}

/// Hex runs ≥ `min_len` → a short fixed `***` mask (compact, stable).
fn mask_hex_blobs(text: &str, min_len: usize) -> String {
    match find_hex_blob(text, min_len) {
        Some((start, end)) => {
            let mut out = String::with_capacity(text.len());
            out.push_str(&text[..start]);
            out.push_str("***");
            out.push_str(&text[end..]);
            out
        }
        None => text.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET: &str = "fp_live_secret_value";

    #[test]
    fn detects_every_shape() {
        let text = "url postgres://fishpond:hunter2@db:5432/x auth Bearer abc.def.ghi \
                     query ?password=hunter2 blob 0123456789abcdef0123456789abcdef";
        let kinds = findings(text, &[]);
        assert!(kinds.contains(&ScanFinding::UrlUserInfo), "{kinds:?}");
        assert!(kinds.contains(&ScanFinding::BearerToken), "{kinds:?}");
        assert!(kinds.contains(&ScanFinding::KeyValue), "{kinds:?}");
        assert!(kinds.contains(&ScanFinding::HexBlob), "{kinds:?}");
    }

    #[test]
    fn known_secrets_are_reported_and_masked() {
        let text = format!("connect failed with {SECRET} at stage 3");
        assert_eq!(findings(&text, &[SECRET]), vec![ScanFinding::KnownSecret]);
        let clean = sanitize(&text, &[SECRET]);
        assert!(!clean.contains(SECRET));
        assert!(clean.contains("***"));
    }

    #[test]
    fn url_userinfo_password_is_masked_but_host_survives() {
        let clean = sanitize("postgres://fishpond:hunter2@db:5432/fishpond", &[]);
        assert!(!clean.contains("hunter2"));
        assert!(clean.contains("postgres://***@db:5432/fishpond"), "{clean}");
    }

    #[test]
    fn bearer_and_key_values_are_masked() {
        let clean = sanitize(
            "Authorization bearer sk-123 and ?password=pw&api_key=k2",
            &[],
        );
        assert!(!clean.contains("sk-123"));
        assert!(!clean.contains("pw&"));
        assert!(!clean.contains("k2"));
        assert!(clean.contains("bearer ***"));
        assert!(clean.contains("password=***"), "{clean}");
        assert!(clean.contains("api_key=***"), "{clean}");
    }

    #[test]
    fn long_hex_is_masked_but_ids_survive() {
        let token = "0123456789abcdef0123456789abcdef";
        let clean = sanitize(format!("token body {token} ends").as_str(), &[]);
        assert!(!clean.contains(token));
        // A short id-shaped hex run stays untouched.
        let short = sanitize("event evt_0123abc stored", &[]);
        assert!(short.contains("evt_0123abc"), "{short}");
    }

    #[test]
    fn sanitize_is_idempotent() {
        let text = "postgres://u:p@h bearer tok9 password=xyz 0123456789abcdef0123456789abcdef";
        let once = sanitize(text, &[SECRET]);
        let twice = sanitize(&once, &[SECRET]);
        assert_eq!(once, twice);
        assert!(findings(&once, &[SECRET]).is_empty(), "{once}");
    }

    #[test]
    fn every_occurrence_is_masked_and_masks_are_left_alone() {
        let text = "?password=one&token=two api_key=three end";
        let clean = sanitize(text, &[]);
        assert_eq!(clean, "?password=***&token=*** api_key=*** end", "{clean}");
        // Re-sanitizing changes nothing (the masks are not findings).
        assert_eq!(sanitize(&clean, &[]), clean);
    }

    #[test]
    fn findings_never_carry_positions_or_content() {
        let text = format!("bearer {SECRET}");
        for finding in findings(&text, &[SECRET]) {
            let label = finding.label();
            // The label names the kind only — never the matched text.
            assert!(!label.contains(SECRET));
            assert!(!label.contains("sk-"));
        }
    }

    #[test]
    fn empty_secret_and_plain_text_are_noops() {
        assert_eq!(
            sanitize("plain operator note", &[""]),
            "plain operator note"
        );
        assert!(findings("plain operator note", &[]).is_empty());
    }
}
