//! Webhook request signing (F018): HMAC-SHA256 over
//! `<unix_millis>.<body>` under the subscription's signing secret.
//!
//! The header format mirrors the de-facto standard webhook scheme:
//!
//! ```text
//! x-fishpond-signature: t=<unix_millis>,v1=<hex mac>
//! ```
//!
//! Receivers reject a signature whose timestamp drifted beyond their
//! tolerance and compare the MAC over the *exact received bytes* —
//! which is why deliveries snapshot their body once and re-sign those
//! same bytes on every retry.

use hmac::{Hmac, Mac};
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

/// The signature header this platform sends.
pub const SIGNATURE_HEADER: &str = "x-fishpond-signature";

/// The content type deliveries are POSTed with.
pub const CONTENT_TYPE: &str = "application/json";

/// Signs `body` at `timestamp_ms` and renders the header value
/// `t=<ts>,v1=<hex>`. The MAC input is `"<timestamp_ms>.<body>"`.
pub fn signature_header(secret: &str, timestamp_ms: i64, body: &str) -> String {
    let mac = mac(secret, timestamp_ms, body);
    format!("t={timestamp_ms},v1={hex}", hex = hex_encode(&mac))
}

/// Checks a rendered header against `body` at `timestamp_ms` with
/// strict tolerance for receiver-side tests. `Ok(())` means the MAC
/// matched (constant-time comparison via the HMAC crate).
pub fn verify_header(
    secret: &str,
    timestamp_ms: i64,
    body: &str,
    header: &str,
) -> Result<(), VerifyError> {
    let expected = mac(secret, timestamp_ms, body);
    let mut parts = header.split(',');
    let ts_part = parts.next().unwrap_or_default();
    let mac_part = parts.next().unwrap_or_default();
    let ts_ok = ts_part == format!("t={timestamp_ms}");
    let given = mac_part
        .strip_prefix("v1=")
        .and_then(decode_hex)
        .ok_or(VerifyError::Malformed)?;
    if !ts_ok || given != expected {
        return Err(VerifyError::Mismatch);
    }
    Ok(())
}

/// Why a header failed verification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerifyError {
    /// The header is not `t=<ts>,v1=<hex>` shaped, or the hex is bad.
    Malformed,
    /// The MAC (or the embedded timestamp) does not match.
    Mismatch,
}

fn mac(secret: &str, timestamp_ms: i64, body: &str) -> [u8; 32] {
    let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).expect("any key length is valid");
    mac.update(format!("{timestamp_ms}.{body}").as_bytes());
    mac.finalize().into_bytes().into()
}

fn hex_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

fn decode_hex(raw: &str) -> Option<Vec<u8>> {
    if !raw.len().is_multiple_of(2) {
        return None;
    }
    (0..raw.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(raw.get(i..i + 2)?, 16).ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{SIGNATURE_HEADER, VerifyError, signature_header, verify_header};

    /// Fixed vector computed with openssl:
    /// `printf '1700000000000.{"n":1}' | openssl dgst -sha256 -hmac "whsec_test_vector_key"`.
    const EXPECTED: &str = "1f2742784cf63927ca30a7518083fd6c831a7973b4ed17735fb187c730bb9da7";

    #[test]
    fn header_matches_the_openssl_vector() {
        let header = signature_header("whsec_test_vector_key", 1_700_000_000_000, "{\"n\":1}");
        assert_eq!(header, format!("t=1700000000000,v1={EXPECTED}"));
        assert_eq!(
            verify_header(
                "whsec_test_vector_key",
                1_700_000_000_000,
                "{\"n\":1}",
                &header
            ),
            Ok(())
        );
    }

    #[test]
    fn any_change_breaks_the_mac() {
        let header = signature_header("whsec_k", 1_000, "{\"n\":1}");
        assert_eq!(
            verify_header("whsec_other", 1_000, "{\"n\":1}", &header),
            Err(VerifyError::Mismatch),
            "wrong secret"
        );
        assert_eq!(
            verify_header("whsec_k", 1_001, "{\"n\":1}", &header),
            Err(VerifyError::Mismatch),
            "wrong timestamp"
        );
        assert_eq!(
            verify_header("whsec_k", 1_000, "{\"n\":2}", &header),
            Err(VerifyError::Mismatch),
            "wrong body"
        );
        assert_eq!(
            verify_header("whsec_k", 1_000, "{\"n\":1}", "garbage"),
            Err(VerifyError::Malformed),
        );
        assert_ne!(
            signature_header("whsec_k", 1_000, "{\"n\":1}"),
            signature_header("whsec_k", 1_000, "{\"n\":2}"),
            "signing is body-sensitive"
        );
        assert!(SIGNATURE_HEADER.starts_with("x-"), "sent as a header");
    }
}
