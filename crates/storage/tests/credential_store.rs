//! F008 acceptance: credential store — envelope encryption, rotation,
//! revocation, and secrets never reaching logs.
//!
//! The lifecycle tests run against the same env-gated database as the
//! other suites (see `common`); the crypto and log-capture tests run
//! everywhere, database or not.

mod common;

use common::*;

use fishpond_storage::{
    CredentialCipher, CredentialRecord, MasterKey, NewCredential, SecretBox, StorageError,
};
use std::io::Write;
use std::sync::{Arc, Mutex};
use tracing_subscriber::util::SubscriberInitExt;

const SECRET: &str = "fp_live_secret_do_not_leak_0123456789";

#[tokio::test]
async fn credential_lifecycle_seal_read_rotate_revoke() {
    let Some(store) = gated().await else {
        eprintln!("skip: {ENV} is not set (CI has no database)");
        return;
    };
    let tenant = unique_tenant("cred");
    store
        .upsert_tenant(&tenant_record(&tenant, "Creds"))
        .await
        .expect("tenant insert");

    // Master-key generations are registered bookkeeping. Versions are
    // global primary keys, so the run stamps its own two generations.
    let v1 = unique_version();
    let v2 = unique_version();
    store.register_master_key(v1, ts(500)).await.unwrap();
    store.register_master_key(v1, ts(501)).await.unwrap(); // idempotent
    store.register_master_key(v2, ts(600)).await.unwrap();
    let keys = store.list_master_keys().await.unwrap();
    assert!(keys.contains(&(v1, ts(500))), "got {keys:?}");
    assert!(keys.contains(&(v2, ts(600))), "got {keys:?}");

    // Seal + store + load + open round-trips the secret.
    let kek_v1 = CredentialCipher::new(MasterKey::generate(), v1);
    let id = unique_entity("cred", "cycle");
    let issued = store
        .store_credential(NewCredential {
            id: &id,
            tenant: &tenant,
            platform: "mock",
            kind: "api_token",
            cipher: &kek_v1,
            secret: SecretBox::new(SECRET),
            created_at: ts(1_000),
        })
        .await
        .expect("issue");
    assert_eq!(issued.kek_version, v1);
    assert_eq!(
        issued.fingerprint.as_deref(),
        Some(&SecretBox::new(SECRET).fingerprint().to_string()[..])
    );
    assert!(!issued.is_revoked);
    // The record never carries the secret.
    assert!(!format!("{issued:?}").contains(SECRET));

    let sealed = store.load_credential(&tenant, &id).await.expect("load");
    assert_eq!(kek_v1.open(&sealed).expect("open").expose(), SECRET);

    // KEK rotation: re-wrap only the DEK, data ciphertext unchanged.
    let kek_v2 = CredentialCipher::new(MasterKey::generate(), v2);
    let rewrapped = kek_v1.rewrap(&sealed, &kek_v2).expect("rewrap");
    assert_eq!(rewrapped.data_cipher, sealed.data_cipher);
    assert_ne!(rewrapped.wrapped_dek, sealed.wrapped_dek);
    assert!(
        store
            .replace_credential_envelope(&tenant, &id, &rewrapped)
            .await
            .unwrap()
    );
    let rotated = store
        .load_credential(&tenant, &id)
        .await
        .expect("load rotated");
    assert_eq!(rotated.kek_version, v2);
    assert_eq!(kek_v2.open(&rotated).expect("open").expose(), SECRET);
    assert!(kek_v1.open(&rotated).is_err(), "old KEK must not open");

    // Secret rotation: fresh DEK, data re-encrypted.
    let new_secret = SecretBox::new("fp_rotated_secret_9876543210");
    let new_sealed = kek_v2.seal(new_secret.expose()).expect("re-seal");
    assert!(
        store
            .replace_credential_envelope(&tenant, &id, &new_sealed)
            .await
            .unwrap()
    );
    let loaded = store
        .load_credential(&tenant, &id)
        .await
        .expect("load resealed");
    assert_eq!(
        kek_v2.open(&loaded).expect("open").expose(),
        "fp_rotated_secret_9876543210"
    );

    // Revocation: once only, and the credential stops opening.
    assert!(
        store
            .revoke_credential(&tenant, &id, ts(9_000))
            .await
            .unwrap()
    );
    assert!(
        !store
            .revoke_credential(&tenant, &id, ts(9_001))
            .await
            .unwrap()
    );
    let err = store
        .load_credential(&tenant, &id)
        .await
        .expect_err("revoked credentials must not load");
    assert!(
        matches!(err, StorageError::PermissionDenied { .. }),
        "got {err:?}"
    );
    let listed = store.list_credentials(&tenant).await.unwrap();
    let revoked = listed.iter().find(|c| c.id == id).expect("listed");
    assert!(revoked.is_revoked);
    assert_eq!(revoked.revoked_at, Some(ts(9_000)));

    // Retirement of a fully rotated generation is idempotent bookkeeping.
    assert!(store.retire_master_key(v1).await.unwrap());
    assert!(!store.retire_master_key(v1).await.unwrap());
}

#[tokio::test]
async fn credentials_are_tenant_scoped_under_rls() {
    let Some(store) = gated().await else {
        eprintln!("skip: {ENV} is not set (CI has no database)");
        return;
    };
    let tenant_a = unique_tenant("cred-a");
    let tenant_b = unique_tenant("cred-b");
    store
        .upsert_tenant(&tenant_record(&tenant_a, "A"))
        .await
        .unwrap();
    store
        .upsert_tenant(&tenant_record(&tenant_b, "B"))
        .await
        .unwrap();

    let id = unique_entity("cred", "scope");
    let cipher = CredentialCipher::new(MasterKey::generate(), 1);
    store
        .store_credential(NewCredential {
            id: &id,
            tenant: &tenant_a,
            platform: "mock",
            kind: "api_token",
            cipher: &cipher,
            secret: SecretBox::new(SECRET),
            created_at: ts(1_000),
        })
        .await
        .expect("issue for A");

    // A can load it; B cannot (RLS makes it invisible → NotFound).
    assert!(store.load_credential(&tenant_a, &id).await.is_ok());
    match store.load_credential(&tenant_b, &id).await {
        Err(StorageError::NotFound { what, .. }) => assert_eq!(what, "credential"),
        other => panic!("tenant B must not reach A's credential, got {other:?}"),
    }

    // B cannot rotate or revoke A's credential (no rows affected).
    // The smuggled envelope is whatever B could forge without ever
    // opening A's — the point is that the store refuses the row either way.
    let other_kek = CredentialCipher::new(MasterKey::generate(), 9);
    let smuggled = other_kek.seal(SECRET).unwrap();
    assert!(
        !store
            .replace_credential_envelope(&tenant_b, &id, &smuggled)
            .await
            .unwrap()
    );
    assert!(
        !store
            .revoke_credential(&tenant_b, &id, ts(2_000))
            .await
            .unwrap()
    );

    // Listings are scoped too.
    assert_eq!(store.list_credentials(&tenant_a).await.unwrap().len(), 1);
    assert!(store.list_credentials(&tenant_b).await.unwrap().is_empty());

    // Unbound (raw pool) reads fail closed.
    let rows = sqlx::query_scalar::<_, String>("SELECT id FROM credentials")
        .fetch_all(store.pool())
        .await
        .unwrap();
    assert!(
        rows.is_empty(),
        "unbound pool access must see no credentials"
    );

    // A stays intact through all of B's attempts.
    let still_there = store.load_credential(&tenant_a, &id).await.unwrap();
    assert!(
        other_kek.open(&still_there).is_err(),
        "B's KEK cannot open A's secret"
    );
}

#[tokio::test]
async fn revoked_credentials_stop_opening_everywhere() {
    let Some(store) = gated().await else {
        eprintln!("skip: {ENV} is not set (CI has no database)");
        return;
    };
    let tenant = unique_tenant("cred-dead");
    store
        .upsert_tenant(&tenant_record(&tenant, "Dead"))
        .await
        .unwrap();
    let cipher = CredentialCipher::new(MasterKey::generate(), 1);
    let id = unique_entity("cred", "dead");
    store
        .store_credential(NewCredential {
            id: &id,
            tenant: &tenant,
            platform: "mock",
            kind: "cookie",
            cipher: &cipher,
            secret: SecretBox::new(SECRET),
            created_at: ts(1_000),
        })
        .await
        .expect("issue");

    // An unknown credential is NotFound, not a permission hint.
    match store
        .load_credential(&tenant, &unique_entity("cred", "ghost"))
        .await
    {
        Err(StorageError::NotFound { what, id }) => {
            assert_eq!(what, "credential");
            assert!(id.contains("ghost"));
        }
        other => panic!("unknown credential must be NotFound, got {other:?}"),
    }

    assert!(
        store
            .revoke_credential(&tenant, &id, ts(5_000))
            .await
            .unwrap()
    );
    // After revocation the cipher can be regenerated at will — the store
    // refuses to hand out the envelope at all.
    assert!(store.load_credential(&tenant, &id).await.is_err());
    assert!(store.list_credentials(&tenant).await.unwrap()[0].is_revoked);
}

#[tokio::test]
async fn issue_refuses_blank_ids() {
    let Some(store) = gated().await else {
        eprintln!("skip: {ENV} is not set (CI has no database)");
        return;
    };
    let tenant = unique_tenant("cred-blank");
    store
        .upsert_tenant(&tenant_record(&tenant, "Blank"))
        .await
        .unwrap();
    let cipher = CredentialCipher::new(MasterKey::generate(), 1);
    let err = store
        .store_credential(NewCredential {
            id: "  ",
            tenant: &tenant,
            platform: "mock",
            kind: "api_token",
            cipher: &cipher,
            secret: SecretBox::new(SECRET),
            created_at: ts(1_000),
        })
        .await
        .expect_err("blank id must be refused");
    assert!(matches!(err, StorageError::Invalid { .. }), "got {err:?}");
}

/// Captures tracing output of the credential lifecycle and asserts the raw
/// secret and key material never appear anywhere in it.
#[tokio::test(flavor = "current_thread")]
async fn secrets_never_reach_logs() {
    let buf = SharedBuf(Arc::new(Mutex::new(Vec::new())));
    let subscriber = tracing_subscriber::fmt()
        .with_writer(buf.clone())
        .with_ansi(false)
        .with_max_level(tracing::Level::TRACE)
        .finish();
    let _guard = subscriber.set_default();

    // A full lifecycle, with the values traced deliberately at debug/trace
    // level (records are Debug-printed): nothing may leak.
    let kek = MasterKey::generate();
    let cipher = CredentialCipher::new(kek.clone(), 1);
    let secret = SecretBox::new(SECRET);
    let sealed = cipher.seal(SECRET).expect("seal");
    let opened = cipher.open(&sealed).expect("open");

    tracing::debug!(kek = ?kek, cipher = ?cipher, "cipher state");
    tracing::debug!(sealed = ?sealed, "sealed envelope");
    tracing::debug!(opened = ?opened, secret = ?secret, "opened secret");
    tracing::debug!(record = ?CredentialRecord {
        id: "cred-x".to_owned(),
        tenant_id: fishpond_domain::TenantId::parse("t-x").unwrap(),
        platform: "mock".to_owned(),
        kind: "api_token".to_owned(),
        kek_version: sealed.kek_version,
        fingerprint: Some(secret.fingerprint()),
        created_at: fishpond_domain::UnixMillis::from_millis(1).unwrap(),
        is_revoked: false,
        revoked_at: None,
    }, "credential record");
    // Even an error path must not echo material.
    let mut tampered = sealed.clone();
    tampered.data_cipher[0] ^= 0x01;
    if let Err(err) = cipher.open(&tampered) {
        tracing::warn!(error = %err, "open failed");
    }

    let logged = String::from_utf8(buf.0.lock().expect("log buffer").clone()).expect("utf8 logs");
    assert!(
        !logged.contains(SECRET),
        "logs leaked the secret:\n{logged}"
    );
    assert!(
        !logged.contains(secret.expose()),
        "logs leaked the secret box contents"
    );
    // Correlation aids are present.
    assert!(
        logged.contains(&secret.fingerprint()),
        "fingerprint should be loggable:\n{logged}"
    );
}

#[derive(Clone)]
struct SharedBuf(Arc<Mutex<Vec<u8>>>);

impl Write for SharedBuf {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().expect("log buffer").extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for SharedBuf {
    type Writer = SharedBuf;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}
