//! F028 acceptance (package side): signing, verification, tamper and
//! canonical-order invariants. Pure — no database, no provider.

use fishpond_provider_sdk::{
    Capability, EngineKind, ManifestError, PackageError, ProtocolVersion, ProviderManifest,
    ProviderPackage, canonical_manifest,
};

fn manifest(version: &str, caps: &[Capability]) -> ProviderManifest {
    ProviderManifest {
        provider_id: "stub".to_owned(),
        display_name: "versioned stub".to_owned(),
        provider_version: version.to_owned(),
        engine: EngineKind::Api,
        protocol_versions: vec![ProtocolVersion::V1_0, ProtocolVersion::V1_1],
        capabilities: caps.to_vec(),
    }
}

#[test]
fn signs_and_verifies_round_trip() {
    let package =
        ProviderPackage::sign(manifest("1.0", &[Capability::Authorize]), "whsec-k").expect("signs");
    package.verify("whsec-k").expect("the same secret verifies");
}

#[test]
fn tampered_content_fails_verification() {
    let mut package =
        ProviderPackage::sign(manifest("1.0", &[Capability::Authorize]), "whsec-k").expect("signs");
    package.manifest.display_name = "tampered".to_owned();
    assert_eq!(package.verify("whsec-k"), Err(PackageError::BadSignature));
}

#[test]
fn wrong_secret_fails_verification() {
    let package =
        ProviderPackage::sign(manifest("1.0", &[Capability::Authorize]), "whsec-k").expect("signs");
    assert_eq!(
        package.verify("whsec-other"),
        Err(PackageError::BadSignature)
    );
}

#[test]
fn blank_secrets_are_rejected() {
    assert_eq!(
        ProviderPackage::sign(manifest("1.0", &[Capability::Authorize]), "   "),
        Err(PackageError::EmptySecret)
    );
    let package =
        ProviderPackage::sign(manifest("1.0", &[Capability::Authorize]), "whsec-k").expect("signs");
    assert_eq!(package.verify(""), Err(PackageError::EmptySecret));
}

#[test]
fn invalid_manifests_are_rejected() {
    let dup = manifest("1.0", &[Capability::Authorize, Capability::Authorize]);
    assert_eq!(
        ProviderPackage::sign(dup, "whsec-k"),
        Err(PackageError::InvalidManifest(
            ManifestError::DuplicateCapability(Capability::Authorize)
        ))
    );
}

#[test]
fn declaration_order_never_changes_the_signature() {
    let a = ProviderManifest {
        protocol_versions: vec![ProtocolVersion::V1_1, ProtocolVersion::V1_0],
        capabilities: vec![Capability::SendMessage, Capability::Authorize],
        ..manifest("1.0", &[])
    };
    let b = ProviderManifest {
        protocol_versions: vec![ProtocolVersion::V1_0, ProtocolVersion::V1_1],
        capabilities: vec![Capability::Authorize, Capability::SendMessage],
        ..manifest("1.0", &[])
    };
    assert_eq!(canonical_manifest(&a), canonical_manifest(&b));
    let signed_a = ProviderPackage::sign(a, "whsec-k").expect("signs");
    let signed_b = ProviderPackage::sign(b, "whsec-k").expect("signs");
    assert_eq!(signed_a.signature, signed_b.signature);
}

#[test]
fn garbage_signatures_are_bad_signatures() {
    let mut package =
        ProviderPackage::sign(manifest("1.0", &[Capability::Authorize]), "whsec-k").expect("signs");
    package.signature = "not-hex-at-all".to_owned();
    assert_eq!(package.verify("whsec-k"), Err(PackageError::BadSignature));
    package.signature = "abc".to_owned();
    assert_eq!(package.verify("whsec-k"), Err(PackageError::BadSignature));
}
