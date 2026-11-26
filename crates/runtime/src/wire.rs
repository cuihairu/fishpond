//! Internal service identity and signed envelopes (F020).
//!
//! Tenant API keys (F007) authenticate *external* callers; this module
//! is the identity plane for *internal* services — browser workers,
//! schedulers — that talk to the control plane. Identities are
//! protobuf messages generated from `proto/fishpond/internal/v1` and
//! carried in an HMAC-SHA256 signed envelope, so a receiver verifies
//! who sent a payload instead of trusting the transport.
//!
//! The generated types are the wire contract. The tests below lock
//! the encoding with golden bytes (an independent Python computation)
//! and check the proto3 compatibility rules from the .proto header:
//! unknown fields are skipped, never fatal; enum value numbers are
//! frozen. Any schema drift breaks this build, not a deployed peer.
//!
//! Like the rest of the platform: explicit instants (`now_ms`
//! parameters), no clock reads, and error text carries field names
//! only — never secrets or payload bytes.

/// Wire types generated from `proto/fishpond/internal/v1` — the
/// compatibility contract. Edit the .proto, never this output.
#[allow(missing_docs)]
pub mod pb {
    include!(concat!(env!("OUT_DIR"), "/fishpond.internal.v1.rs"));
}

use hmac::{Hmac, Mac};
use prost::Message;
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

use pb::{ServiceIdentity, ServiceKind, SignedEnvelope};

/// Why an envelope was refused. Variants carry static field names
/// only — no secret material and no payload bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdentityError {
    /// The envelope is not usable: an empty `mac` (verify only),
    /// `service_id` or `payload_type`, or a `kind` that does not map
    /// to a known [`ServiceKind`].
    Malformed(&'static str),
    /// The MAC does not match: wrong secret or tampered bytes.
    Mismatch,
    /// `now_ms` is at or past the identity's `expires_at_ms`.
    Expired,
    /// `issued_at_ms` is in the future relative to `now_ms` — clock
    /// skew or a forged identity; fail closed either way.
    NotYetValid,
}

impl std::fmt::Display for IdentityError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            IdentityError::Malformed(what) => {
                write!(f, "envelope malformed: {what} is missing or unknown")
            }
            IdentityError::Mismatch => write!(f, "envelope MAC mismatch"),
            IdentityError::Expired => write!(f, "identity expired"),
            IdentityError::NotYetValid => write!(f, "identity issued in the future"),
        }
    }
}

impl std::error::Error for IdentityError {}

/// The identity every signed envelope must carry. None of it is a
/// credential: possession of the HMAC secret is what proves the
/// sender, the identity only names it.
fn validate_shape(envelope: &SignedEnvelope) -> Result<(), IdentityError> {
    if envelope.payload_type.is_empty() {
        return Err(IdentityError::Malformed("payload_type"));
    }
    if envelope.identity.is_none() {
        return Err(IdentityError::Malformed("identity"));
    }
    let identity = envelope.identity.as_ref().expect("checked above");
    if identity.service_id.is_empty() {
        return Err(IdentityError::Malformed("service_id"));
    }
    if ServiceKind::try_from(identity.kind).is_err() {
        return Err(IdentityError::Malformed("kind"));
    }
    if identity.issued_at_ms < 0 || identity.expires_at_ms < 0 {
        return Err(IdentityError::Malformed("validity window"));
    }
    Ok(())
}

/// The MAC input: the envelope's own encoding with `mac` left empty.
/// Reusing the wire encoder keeps the MAC input canonical — no
/// ad-hoc framing to get wrong.
fn mac_input(envelope: &SignedEnvelope) -> Vec<u8> {
    let mut unsigned = envelope.clone();
    unsigned.mac = Vec::new();
    unsigned.encode_to_vec()
}

fn mac(secret: &[u8], input: &[u8]) -> [u8; 32] {
    let mut mac = HmacSha256::new_from_slice(secret).expect("any key length is valid");
    mac.update(input);
    mac.finalize().into_bytes().into()
}

/// Signs `envelope`: fills its `mac` with an HMAC-SHA256 over the
/// rest of the message. Fails closed on a shape that could never be
/// verified (empty identity/type) — signing garbage is a caller bug.
pub fn sign_envelope(
    secret: &[u8],
    mut envelope: SignedEnvelope,
) -> Result<SignedEnvelope, IdentityError> {
    envelope.mac = Vec::new();
    validate_shape(&envelope)?;
    envelope.mac = mac(secret, &mac_input(&envelope)).to_vec();
    Ok(envelope)
}

/// Verifies an envelope: MAC first (constant-time), then the
/// validity window at `now_ms`. Returns the verified identity —
/// callers still enforce their own nonce freshness and tenant
/// binding policy.
pub fn verify_envelope(
    secret: &[u8],
    envelope: &SignedEnvelope,
    now_ms: i64,
) -> Result<ServiceIdentity, IdentityError> {
    if envelope.mac.is_empty() {
        return Err(IdentityError::Malformed("mac"));
    }
    validate_shape(envelope)?;
    let expected = mac(secret, &mac_input(envelope));
    if envelope.mac != expected.to_vec() {
        return Err(IdentityError::Mismatch);
    }
    let identity = envelope.identity.clone().expect("checked above");
    if identity.issued_at_ms > now_ms {
        return Err(IdentityError::NotYetValid);
    }
    if identity.expires_at_ms <= now_ms {
        return Err(IdentityError::Expired);
    }
    Ok(identity)
}

#[cfg(test)]
mod tests {
    use super::pb::{ServiceIdentity, ServiceKind, SignedEnvelope};
    use super::{IdentityError, sign_envelope, verify_envelope};
    use hex;
    use prost::Message;

    const SECRET: &[u8] = b"whsec_f020_vector";

    /// The canonical fixture: values chosen so every field encodes
    /// (no proto3 defaults hiding a tag).
    fn fixture_identity() -> ServiceIdentity {
        ServiceIdentity {
            service_id: "worker-1".to_owned(),
            kind: ServiceKind::Worker.into(),
            tenant_ids: vec!["t-a".to_owned(), "t-b".to_owned()],
            issued_at_ms: 1_000,
            expires_at_ms: 2_000,
            generation: 7,
        }
    }

    fn fixture_envelope() -> SignedEnvelope {
        SignedEnvelope {
            identity: Some(fixture_identity()),
            payload_type: "fishpond.internal.v1.Heartbeat".to_owned(),
            payload: vec![1, 2, 3],
            nonce: 42,
            mac: Vec::new(),
        }
    }

    /// Golden bytes computed with an independent Python encoder —
    /// the wire format this build promises to keep.
    const IDENTITY_GOLDEN: &str = "0a08776f726b65722d3110011a03742d611a03742d6220e80728d00f3007";
    const ENVELOPE_GOLDEN: &str = "0a1e0a08776f726b65722d3110011a03742d611a03742d6220e80728d00f3007\
         121e66697368706f6e642e696e7465726e616c2e76312e486561727462656174\
         1a03010203202a";
    const MAC_GOLDEN: &str = "88a768f96c2f82899a89093879d4992cbcfc7d8105e0b0a3eab960190fe2ff69";

    #[test]
    fn golden_bytes_lock_the_identity_encoding() {
        assert_eq!(
            hex::encode(fixture_identity().encode_to_vec()),
            IDENTITY_GOLDEN,
            "identity wire format drifted"
        );
        assert_eq!(
            hex::encode(fixture_envelope().encode_to_vec()),
            ENVELOPE_GOLDEN,
            "envelope wire format drifted"
        );
    }

    #[test]
    fn mac_matches_the_independent_vector() {
        let signed = sign_envelope(SECRET, fixture_envelope()).expect("sign");
        assert_eq!(
            hex::encode(&signed.mac),
            MAC_GOLDEN,
            "MAC input or scheme drifted"
        );
    }

    #[test]
    fn round_trip_reencodes_the_same_bytes() {
        let identity =
            ServiceIdentity::decode(hex::decode(IDENTITY_GOLDEN).expect("hex").as_slice())
                .expect("golden decodes");
        assert_eq!(hex::encode(identity.encode_to_vec()), IDENTITY_GOLDEN);
        assert_eq!(identity, fixture_identity());
    }

    #[test]
    fn unknown_fields_are_skipped_not_fatal() {
        // Field 15, varint 42 — a field a newer producer added.
        let mut bytes = hex::decode(IDENTITY_GOLDEN).expect("hex");
        bytes.extend_from_slice(&[0x78, 0x2a]);
        let identity = ServiceIdentity::decode(bytes.as_slice()).expect("unknown fields skip");
        assert_eq!(identity, fixture_identity(), "known fields intact");
    }

    #[test]
    fn enum_wire_values_are_frozen() {
        // Tag 2, varint values 1 and 2 must keep meaning worker and
        // scheduler; an unmapped value decodes but fails verification.
        for (raw, want) in [(1, ServiceKind::Worker), (2, ServiceKind::Scheduler)] {
            let bytes = [0x10, raw];
            let identity = ServiceIdentity::decode(bytes.as_slice()).expect("decode");
            assert_eq!(ServiceKind::try_from(identity.kind), Ok(want));
        }
        let mut unknown = ServiceIdentity::decode(&[0x10, 0x09][..]).expect("open enum decodes");
        unknown.service_id = "worker-1".to_owned();
        let envelope = SignedEnvelope {
            identity: Some(unknown),
            payload_type: "t".to_owned(),
            payload: vec![],
            nonce: 1,
            mac: vec![0xaa; 32],
        };
        assert_eq!(
            verify_envelope(SECRET, &envelope, 5_000),
            Err(IdentityError::Malformed("kind"))
        );
    }

    #[test]
    fn envelope_verification_binds_every_byte() {
        let signed = sign_envelope(SECRET, fixture_envelope()).expect("sign");
        let identity =
            verify_envelope(SECRET, &signed, 1_500).expect("in-window envelope verifies");
        assert_eq!(identity, fixture_identity());

        // Wrong secret, tampered payload, bumped generation, rotated
        // nonce: every one of them is a Mismatch, not a pass.
        assert_eq!(
            verify_envelope(b"whsec_other", &signed, 1_500),
            Err(IdentityError::Mismatch)
        );
        let mut tampered = signed.clone();
        tampered.payload[2] ^= 0x01;
        assert_eq!(
            verify_envelope(SECRET, &tampered, 1_500),
            Err(IdentityError::Mismatch)
        );
        let mut forged = signed.clone();
        forged.identity.as_mut().expect("identity").generation += 1;
        assert_eq!(
            verify_envelope(SECRET, &forged, 1_500),
            Err(IdentityError::Mismatch)
        );
        let mut fresh = signed;
        fresh.nonce += 1;
        assert_eq!(
            verify_envelope(SECRET, &fresh, 1_500),
            Err(IdentityError::Mismatch)
        );
    }

    #[test]
    fn validity_window_fails_closed_at_both_ends() {
        let signed = sign_envelope(SECRET, fixture_envelope()).expect("sign");
        // issued 1000, expires 2000.
        assert_eq!(
            verify_envelope(SECRET, &signed, 999),
            Err(IdentityError::NotYetValid)
        );
        assert!(verify_envelope(SECRET, &signed, 1_000).is_ok());
        assert!(verify_envelope(SECRET, &signed, 1_999).is_ok());
        assert_eq!(
            verify_envelope(SECRET, &signed, 2_000),
            Err(IdentityError::Expired)
        );
    }

    #[test]
    fn malformed_envelopes_are_rejected() {
        let mut envelope = fixture_envelope();
        envelope.mac = vec![0xaa; 32];
        envelope.identity.as_mut().expect("identity").service_id = String::new();
        assert_eq!(
            sign_envelope(SECRET, envelope.clone()),
            Err(IdentityError::Malformed("service_id"))
        );

        let mut no_type = fixture_envelope();
        no_type.payload_type = String::new();
        no_type.mac = vec![0xaa; 32];
        assert_eq!(
            verify_envelope(SECRET, &no_type, 1_500),
            Err(IdentityError::Malformed("payload_type"))
        );

        let unsigned = fixture_envelope();
        assert_eq!(
            verify_envelope(SECRET, &unsigned, 1_500),
            Err(IdentityError::Malformed("mac"))
        );

        let mut no_identity = fixture_envelope();
        no_identity.identity = None;
        no_identity.mac = vec![0xaa; 32];
        assert_eq!(
            verify_envelope(SECRET, &no_identity, 1_500),
            Err(IdentityError::Malformed("identity"))
        );

        let mut negative = fixture_envelope();
        negative.identity.as_mut().expect("identity").expires_at_ms = -1;
        negative.mac = vec![0xaa; 32];
        assert_eq!(
            verify_envelope(SECRET, &negative, 1_500),
            Err(IdentityError::Malformed("validity window"))
        );
    }

    #[test]
    fn error_text_carries_no_secrets_or_payloads() {
        let text = IdentityError::Mismatch.to_string();
        assert!(!text.to_lowercase().contains("mac=") && !text.contains("secret"));
        let signed = sign_envelope(SECRET, fixture_envelope()).expect("sign");
        let debug = format!("{signed:?}");
        assert!(
            !debug.contains("whsec") && !debug.contains("Heartbeat-body"),
            "envelope debug must not look like it carries the secret: {debug}"
        );
    }
}
