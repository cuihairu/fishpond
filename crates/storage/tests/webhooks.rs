//! F018 acceptance (storage side): webhook subscriptions and the
//! delivery queue.
//!
//! Subscriptions seal their signing secret before the database (F008
//! envelope); the delivery queue claims due rows atomically (including
//! stale `delivering` leftovers), settles them by transport outcome,
//! and `replay_delivery` revives any row with a fresh budget. Both
//! tables are tenant-isolated like the rest of the schema.

mod common;

use common::{ENV, gated, tenant_record, ts, unique_entity, unique_tenant, unique_version};
use fishpond_domain::EventId;
use fishpond_storage::{
    CredentialCipher, DeliveryStatus, MasterKey, NewDelivery, NewWebhookSubscription, SecretBox,
};
use sqlx::Row;

const SECRET: &str = "whsec_test_signing_material_not_for_logs";

async fn tenant_for(store: &fishpond_storage::Store, tag: &str) -> fishpond_domain::TenantId {
    let tenant = unique_tenant(tag);
    store
        .upsert_tenant(&tenant_record(&tenant, tag))
        .await
        .expect("tenant insert");
    tenant
}

async fn cipher_for(store: &fishpond_storage::Store) -> CredentialCipher {
    let version = unique_version();
    store
        .register_master_key(version, ts(500))
        .await
        .expect("master key registration");
    CredentialCipher::new(MasterKey::generate(), version)
}

async fn subscribe(
    store: &fishpond_storage::Store,
    tenant: &fishpond_domain::TenantId,
    id: &str,
    url: &str,
    secret: &str,
    event_type: Option<&str>,
    cipher: &CredentialCipher,
) -> fishpond_storage::WebhookSubscriptionRecord {
    store
        .upsert_webhook_subscription(&NewWebhookSubscription {
            id,
            tenant,
            url,
            cipher,
            secret: SecretBox::new(secret),
            event_type,
            at: ts(1_000),
        })
        .await
        .expect("subscription insert")
}

async fn delivery_for(
    store: &fishpond_storage::Store,
    tenant: &fishpond_domain::TenantId,
    subscription: &str,
    tag: &str,
    seq: u64,
    at: i64,
) -> fishpond_storage::DeliveryRecord {
    let event_id = EventId::parse(unique_entity("evt", tag)).expect("event id");
    store
        .insert_delivery(&NewDelivery {
            id: &unique_entity("del", tag),
            tenant,
            subscription_id: subscription,
            event_id: &event_id,
            event_seq: seq,
            body: r#"{"event":{"seq":1}}"#,
            at: ts(at),
        })
        .await
        .expect("delivery insert")
}

#[tokio::test]
async fn subscription_seals_rotates_and_toggles() {
    let Some(store) = gated().await else {
        eprintln!("skip: {ENV} is not set (CI has no database)");
        return;
    };
    let tenant = tenant_for(&store, "wh-seal").await;
    let cipher = cipher_for(&store).await;

    let id = unique_entity("wh", "seal-a");
    let sub = subscribe(
        &store,
        &tenant,
        &id,
        "https://hooks.example.com/fish",
        SECRET,
        None,
        &cipher,
    )
    .await;
    assert_eq!(sub.url, "https://hooks.example.com/fish");
    assert!(sub.is_enabled);
    assert!(sub.event_type.is_none());
    let fingerprint = sub.fingerprint.clone().expect("fingerprint present");
    assert!(
        !fingerprint.contains(SECRET),
        "fingerprint never carries the secret"
    );
    assert_eq!(sub.kek_version, cipher.kek_version());

    // Rotation: same id, new URL and secret; the enabled state survives.
    store
        .set_webhook_enabled(&tenant, &id, false, ts(2_000))
        .await
        .expect("disable");
    let rotated = subscribe(
        &store,
        &tenant,
        &id,
        "https://hooks.example.com/v2",
        "whsec_rotated_signing_material_also_not_for_logs",
        Some("message.sent"),
        &cipher,
    )
    .await;
    assert_eq!(rotated.id, id);
    assert_eq!(rotated.url, "https://hooks.example.com/v2");
    assert_eq!(rotated.event_type.as_deref(), Some("message.sent"));
    assert!(!rotated.is_enabled, "rotation keeps the enabled state");
    assert_ne!(rotated.fingerprint, Some(fingerprint), "the secret changed");

    // The stored secret opens back to the exact (rotated) material.
    let opened = store
        .open_webhook_secret(&tenant, &id, &cipher)
        .await
        .expect("open");
    assert_eq!(
        opened.expose(),
        "whsec_rotated_signing_material_also_not_for_logs"
    );

    // Disabling and re-enabling round-trips.
    store
        .set_webhook_enabled(&tenant, &id, true, ts(2_100))
        .await
        .expect("enable");
    let listed = store
        .list_webhook_subscriptions(&tenant)
        .await
        .expect("list");
    assert_eq!(listed.len(), 1);
    assert!(listed[0].is_enabled);
    assert!(!listed[0].fingerprint.clone().expect("fp").contains(SECRET));

    // Deleting removes the row; a second delete is NotFound.
    store
        .delete_webhook_subscription(&tenant, &id)
        .await
        .expect("delete");
    let err = store
        .delete_webhook_subscription(&tenant, &id)
        .await
        .expect_err("second delete");
    assert!(err.to_string().contains("webhook subscription"), "{err}");
}

#[tokio::test]
async fn delivery_claim_is_due_only_and_stale_aware() {
    let Some(store) = gated().await else {
        eprintln!("skip: {ENV} is not set (CI has no database)");
        return;
    };
    let tenant = tenant_for(&store, "wh-claim").await;
    let cipher = cipher_for(&store).await;
    let sub = subscribe(
        &store,
        &tenant,
        &unique_entity("wh", "claim"),
        "https://hooks.example.com/claim",
        SECRET,
        None,
        &cipher,
    )
    .await;

    let due = delivery_for(&store, &tenant, &sub.id, "claim-due", 1, 1_000).await;
    let future = delivery_for(&store, &tenant, &sub.id, "claim-future", 2, 5_000).await;

    // Only the due row is claimed; it flips to `delivering`.
    let claimed = store
        .claim_due_deliveries(&tenant, ts(2_000), 60_000, 10)
        .await
        .expect("claim");
    assert_eq!(claimed.len(), 1, "the future row stays queued");
    assert_eq!(claimed[0].id, due.id);
    assert_eq!(claimed[0].status, DeliveryStatus::Delivering);

    // A second claim inside the stale window returns nothing.
    let again = store
        .claim_due_deliveries(&tenant, ts(2_000), 60_000, 10)
        .await
        .expect("second claim");
    assert!(
        again.is_empty(),
        "no double delivery inside the stale window"
    );

    // After the stale window the crashed-loop claim is reclaimable.
    let reclaimed = store
        .claim_due_deliveries(&tenant, ts(2_002), 1, 10)
        .await
        .expect("reclaim");
    assert_eq!(reclaimed.len(), 1, "the stale `delivering` row comes back");
    assert_eq!(reclaimed[0].id, due.id);

    // Back to pending with a future schedule: not claimable until due.
    store
        .settle_retry(&tenant, &due.id, 1, ts(9_999), Some("timeout"), ts(2_100))
        .await
        .expect("back to pending");
    let rows = store
        .claim_due_deliveries(&tenant, ts(3_000), 60_000, 10)
        .await
        .expect("claim at 3_000");
    assert!(rows.is_empty(), "neither row is due yet");

    // Both rows are claimable once their times come.
    let rows = store
        .claim_due_deliveries(&tenant, ts(10_000), 60_000, 10)
        .await
        .expect("claim later");
    let ids: Vec<&str> = rows.iter().map(|d| d.id.as_str()).collect();
    assert!(ids.contains(&due.id.as_str()), "retry due at 9_999");
    assert!(
        ids.contains(&future.id.as_str()),
        "first attempt due at 5_000"
    );
}

#[tokio::test]
async fn delivery_settle_paths_and_replay() {
    let Some(store) = gated().await else {
        eprintln!("skip: {ENV} is not set (CI has no database)");
        return;
    };
    let tenant = tenant_for(&store, "wh-settle").await;
    let cipher = cipher_for(&store).await;
    let sub = subscribe(
        &store,
        &tenant,
        &unique_entity("wh", "settle"),
        "https://hooks.example.com/settle",
        SECRET,
        None,
        &cipher,
    )
    .await;

    let ok = delivery_for(&store, &tenant, &sub.id, "settle-ok", 1, 1_000).await;
    let retry = delivery_for(&store, &tenant, &sub.id, "settle-retry", 2, 1_000).await;
    let dead = delivery_for(&store, &tenant, &sub.id, "settle-dead", 3, 1_000).await;

    // Delivered: terminal, never re-claims.
    store
        .settle_delivered(&tenant, &ok.id, ts(1_100))
        .await
        .expect("settle delivered");

    // Retry: parked until its schedule says so.
    store
        .settle_retry(
            &tenant,
            &retry.id,
            1,
            ts(6_000),
            Some("upstream 503"),
            ts(1_100),
        )
        .await
        .expect("settle retry");

    // Claim what is due: only the not-yet-settled row.
    let rows = store
        .claim_due_deliveries(&tenant, ts(1_200), 60_000, 10)
        .await
        .expect("claim");
    let ids: Vec<&str> = rows.iter().map(|d| d.id.as_str()).collect();
    assert_eq!(
        ids,
        vec![dead.id.as_str()],
        "delivered is terminal, retry is parked"
    );

    // Abandoned (after its claim): terminal without a replay.
    store
        .settle_abandoned(&tenant, &dead.id, Some("upstream 410 gone"), ts(1_300))
        .await
        .expect("settle abandoned");
    let rows = store
        .claim_due_deliveries(&tenant, ts(6_001), 60_000, 10)
        .await
        .expect("claim at 6_001");
    let ids: Vec<&str> = rows.iter().map(|d| d.id.as_str()).collect();
    assert_eq!(
        ids,
        vec![retry.id.as_str()],
        "abandoned never re-claims; retry due at 6_000"
    );
    assert_eq!(rows[0].attempts, 1, "the stored attempt count survives");

    // Replay (重投) revives the abandoned row with a fresh budget.
    let revived = store
        .replay_delivery(&tenant, &dead.id, ts(7_000))
        .await
        .expect("replay");
    assert_eq!(revived.status, DeliveryStatus::Pending);
    assert_eq!(revived.attempts, 0, "fresh retry budget");
    assert_eq!(revived.next_attempt_at, ts(7_000));
    assert!(revived.last_error.is_none());
    let rows = store
        .claim_due_deliveries(&tenant, ts(7_001), 60_000, 10)
        .await
        .expect("claim revived");
    assert!(
        rows.iter().any(|d| d.id == dead.id),
        "the replayed row is due again"
    );

    // Replay works on a delivered row too (manual re-send), and a
    // missing id is NotFound.
    let re_sent = store
        .replay_delivery(&tenant, &ok.id, ts(7_100))
        .await
        .expect("replay delivered");
    assert_eq!(re_sent.status, DeliveryStatus::Pending);
    let err = store
        .replay_delivery(&tenant, "del-missing", ts(7_200))
        .await
        .expect_err("missing replay");
    assert!(err.to_string().contains("webhook delivery"), "{err}");
}

#[tokio::test]
async fn webhook_tables_are_tenant_isolated() {
    let Some(store) = gated().await else {
        eprintln!("skip: {ENV} is not set (CI has no database)");
        return;
    };
    let tenant = tenant_for(&store, "wh-iso-a").await;
    let other = tenant_for(&store, "wh-iso-b").await;
    let cipher = cipher_for(&store).await;
    let sub = subscribe(
        &store,
        &tenant,
        &unique_entity("wh", "iso"),
        "https://hooks.example.com/iso",
        SECRET,
        None,
        &cipher,
    )
    .await;
    delivery_for(&store, &tenant, &sub.id, "iso", 1, 1_000).await;

    // The bare pool sees nothing in either table.
    for table in ["webhook_subscriptions", "webhook_deliveries"] {
        let bare: i64 = sqlx::query(&format!("SELECT count(*) FROM {table}"))
            .fetch_one(store.pool())
            .await
            .expect("bare count")
            .try_get("count")
            .expect("count column");
        assert_eq!(bare, 0, "{table} leaks no tenant rows to the bare pool");
    }

    // A foreign tenant's lists are empty and its claims touch nothing.
    assert!(
        store
            .list_webhook_subscriptions(&other)
            .await
            .expect("list")
            .is_empty()
    );
    let rows = store
        .claim_due_deliveries(&other, ts(2_000), 0, 10)
        .await
        .expect("foreign claim");
    assert!(rows.is_empty());

    // The owner sees and claims its own row.
    assert_eq!(
        store
            .list_webhook_subscriptions(&tenant)
            .await
            .expect("list")
            .len(),
        1
    );
    let rows = store
        .claim_due_deliveries(&tenant, ts(2_000), 0, 10)
        .await
        .expect("own claim");
    assert_eq!(rows.len(), 1);
}
