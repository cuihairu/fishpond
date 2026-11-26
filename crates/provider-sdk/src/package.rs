//! F028 signed provider packages: a manifest plus an HMAC-SHA256
//! signature over its canonical form. Registration faces verify the
//! signature before a provider can serve, so a tampered or mismatched
//! manifest never reaches the routing table.
//!
//! The canonical form sorts protocol versions and capabilities, so two
//! manifests that differ only in declaration order sign identically.

use hmac::{Hmac, Mac};
use sha2::Sha256;

use crate::manifest::{ManifestError, ProviderManifest};

type HmacSha256 = Hmac<Sha256>;

/// Why a package was rejected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PackageError {
    /// The manifest inside the package fails its own invariants.
    InvalidManifest(ManifestError),
    /// The signature is missing, malformed, or does not match the
    /// content (tampered manifest or wrong key).
    BadSignature,
    /// The signing secret was empty or blank.
    EmptySecret,
}

impl std::fmt::Display for PackageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidManifest(e) => write!(f, "package manifest invalid: {e}"),
            Self::BadSignature => f.write_str("package signature does not match the content"),
            Self::EmptySecret => f.write_str("signing secret must not be empty"),
        }
    }
}

impl std::error::Error for PackageError {}

/// A signed provider manifest: what a distributor ships and an operator
/// verifies before registering the provider.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderPackage {
    /// The manifest being attested.
    pub manifest: ProviderManifest,
    /// Lowercase hex HMAC-SHA256 over [`canonical_manifest`].
    pub signature: String,
}

/// Field separator of the canonical form (ASCII unit separator — never
/// appears in the validated string fields).
const SEP: char = '\u{1f}';

/// The deterministic, order-insensitive encoding the signature covers.
///
/// `fishpond-provider-manifest/v1 ␟ provider_id ␟ display_name ␟
/// provider_version ␟ engine ␟ versions=v…,v… ␟ caps=a,b,…` with
/// versions and capabilities sorted, so declaration order cannot change
/// a signature.
pub fn canonical_manifest(manifest: &ProviderManifest) -> String {
    let mut versions = manifest.protocol_versions.clone();
    versions.sort_unstable();
    let versions = versions
        .iter()
        .map(|v| v.to_string())
        .collect::<Vec<_>>()
        .join(",");
    let mut caps = manifest.capabilities.clone();
    caps.sort_unstable();
    let caps = caps
        .iter()
        .map(|c| c.as_str())
        .collect::<Vec<_>>()
        .join(",");
    format!(
        "fishpond-provider-manifest/v1{SEP}{}{SEP}{}{SEP}{}{SEP}{}{SEP}versions={versions}{SEP}caps={caps}",
        manifest.provider_id,
        manifest.display_name,
        manifest.provider_version,
        manifest.engine.as_str(),
    )
}

impl ProviderPackage {
    /// Validates the manifest and signs its canonical form.
    pub fn sign(manifest: ProviderManifest, secret: &str) -> Result<Self, PackageError> {
        if secret.trim().is_empty() {
            return Err(PackageError::EmptySecret);
        }
        manifest.validate().map_err(PackageError::InvalidManifest)?;
        Ok(Self {
            signature: hex_encode(&mac_of(&canonical_manifest(&manifest), secret)),
            manifest,
        })
    }

    /// Verifies the signature against the content under `secret`.
    /// Constant-time comparison (HMAC verify); a tampered manifest or a
    /// wrong key both land here as [`PackageError::BadSignature`].
    pub fn verify(&self, secret: &str) -> Result<(), PackageError> {
        if secret.trim().is_empty() {
            return Err(PackageError::EmptySecret);
        }
        self.manifest
            .validate()
            .map_err(PackageError::InvalidManifest)?;
        let mut mac = HmacSha256::new_from_slice(secret.as_bytes())
            .expect("any key length is valid for HMAC");
        mac.update(canonical_manifest(&self.manifest).as_bytes());
        let expected = decode_hex(&self.signature).ok_or(PackageError::BadSignature)?;
        mac.verify_slice(&expected)
            .map_err(|_| PackageError::BadSignature)
    }
}

fn mac_of(content: &str, secret: &str) -> [u8; 32] {
    let mut mac =
        HmacSha256::new_from_slice(secret.as_bytes()).expect("any key length is valid for HMAC");
    mac.update(content.as_bytes());
    mac.finalize().into_bytes().into()
}

fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn decode_hex(raw: &str) -> Option<Vec<u8>> {
    let raw = raw.trim();
    if !raw.len().is_multiple_of(2) {
        return None;
    }
    (0..raw.len() / 2)
        .map(|i| u8::from_str_radix(raw.get(i * 2..i * 2 + 2)?, 16).ok())
        .collect()
}
