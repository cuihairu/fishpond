//! F006 acceptance: PostgreSQL migrations and tenant-scoped repositories.
//!
//! Tests run against a real database when `FISHPOND_TEST_DATABASE_URL` is
//! set (e.g.
//! `postgres://fishpond_test:fishpond_test@127.0.0.1:5432/fishpond_test`,
//! a non-superuser role so F007's row-level security applies to the
//! application). Without the variable they skip, because CI has no
//! database and Mock is the only CI target (§7.3). Since F007 these calls
//! also prove the store keeps binding every call to its own tenant.

mod common;

use common::*;

use fishpond_domain::{
    AccountState, Conversation, ConversationId, Message, MessageDirection, Product, ProductId,
    ProductStatus, TenantId,
};
use fishpond_storage::{StorageError, Store};

#[tokio::test]
async fn migrations_create_the_core_schema_and_are_idempotent() {
    let Some(store) = gated().await else {
        eprintln!("skip: {ENV} is not set (CI has no database)");
        return;
    };
    // A second run must be a no-op (already-applied versions are skipped).
    store
        .run_migrations()
        .await
        .expect("migrations are idempotent");

    let tables = sqlx::query_scalar::<_, String>(
        "SELECT table_name FROM information_schema.tables
         WHERE table_schema = 'public'
           AND table_name IN ('tenants', 'accounts', 'conversations', 'messages', 'products')
         ORDER BY table_name",
    )
    .fetch_all(store.pool())
    .await
    .expect("query information_schema");
    assert_eq!(
        tables,
        [
            "accounts",
            "conversations",
            "messages",
            "products",
            "tenants"
        ]
    );

    let indexes = sqlx::query_scalar::<_, String>(
        "SELECT indexname FROM pg_indexes
         WHERE schemaname = 'public' AND indexname LIKE 'idx_%' ORDER BY indexname",
    )
    .fetch_all(store.pool())
    .await
    .expect("query pg_indexes");
    assert!(indexes.contains(&"idx_accounts_tenant".to_owned()));
    assert!(indexes.contains(&"idx_conversations_tenant_account".to_owned()));
    assert!(indexes.contains(&"idx_messages_conversation_time".to_owned()));
    assert!(indexes.contains(&"idx_products_tenant".to_owned()));
}

#[tokio::test]
async fn tenant_upsert_lookup_and_not_found() {
    let Some(store) = gated().await else {
        eprintln!("skip: {ENV} is not set (CI has no database)");
        return;
    };
    let tenant = unique_tenant("tenants");
    store
        .upsert_tenant(&tenant_record(&tenant, "Contract Tenant"))
        .await
        .expect("tenant insert");

    // Upsert again with a new name: same id, updated row.
    store
        .upsert_tenant(&tenant_record(&tenant, "Renamed Tenant"))
        .await
        .expect("tenant upsert");
    let loaded = store.get_tenant(&tenant).await.expect("tenant found");
    assert_eq!(loaded.name, "Renamed Tenant");
    assert_eq!(loaded.id, tenant);

    let missing = TenantId::parse("t-no-such-tenant").unwrap();
    match store.get_tenant(&missing).await {
        Err(StorageError::NotFound { what, id }) => {
            assert_eq!(what, "tenant");
            assert_eq!(id, "t-no-such-tenant");
        }
        other => panic!("expected NotFound, got {other:?}"),
    }
}

#[tokio::test]
async fn account_state_updates_and_tenant_scoped_listing() {
    let Some(store) = gated().await else {
        eprintln!("skip: {ENV} is not set (CI has no database)");
        return;
    };
    let tenant_a = unique_tenant("acc-a");
    let tenant_b = unique_tenant("acc-b");
    store
        .upsert_tenant(&tenant_record(&tenant_a, "A"))
        .await
        .unwrap();
    store
        .upsert_tenant(&tenant_record(&tenant_b, "B"))
        .await
        .unwrap();

    let account_a = account(
        &unique_entity("acc", "list-a"),
        &tenant_a,
        AccountState::Online,
        None,
    );
    let account_b = account(
        &unique_entity("acc", "list-b"),
        &tenant_b,
        AccountState::Offline,
        None,
    );
    store.upsert_account(&account_a).await.unwrap();
    store.upsert_account(&account_b).await.unwrap();

    let listed_a = store.list_accounts(&tenant_a).await.unwrap();
    assert_eq!(
        listed_a,
        vec![account_a.clone()],
        "tenant A sees only its row"
    );
    let listed_b = store.list_accounts(&tenant_b).await.unwrap();
    assert_eq!(
        listed_b,
        vec![account_b.clone()],
        "tenant B sees only its row"
    );

    // ONLINE -> UNKNOWN needs a note; the updated row round-trips it.
    let updated = store
        .update_account_state(
            &tenant_a,
            &account_a.id,
            AccountState::Unknown,
            Some("reconcile pending".to_owned()),
            ts(3_000),
        )
        .await
        .unwrap();
    assert_eq!(updated.state, AccountState::Unknown);
    assert_eq!(updated.state_note.as_deref(), Some("reconcile pending"));
    assert_eq!(updated.updated_at, ts(3_000));
    let reread = store.get_account(&tenant_a, &account_a.id).await.unwrap();
    assert_eq!(reread, updated);
}

#[tokio::test]
async fn conversations_scope_by_tenant_and_account() {
    let Some(store) = gated().await else {
        eprintln!("skip: {ENV} is not set (CI has no database)");
        return;
    };
    let tenant = unique_tenant("conv");
    store
        .upsert_tenant(&tenant_record(&tenant, "Conv"))
        .await
        .unwrap();
    let owner = account(
        &unique_entity("acc", "conv-owner"),
        &tenant,
        AccountState::Online,
        None,
    );
    let other = account(
        &unique_entity("acc", "conv-other"),
        &tenant,
        AccountState::Online,
        None,
    );
    store.upsert_account(&owner).await.unwrap();
    store.upsert_account(&other).await.unwrap();

    let conv_a = Conversation {
        id: ConversationId::parse(unique_entity("conv", "list-a")).unwrap(),
        account_id: owner.id.clone(),
        tenant_id: tenant.clone(),
        updated_at: ts(2_100),
    };
    let conv_b = Conversation {
        id: ConversationId::parse(unique_entity("conv", "list-b")).unwrap(),
        account_id: owner.id.clone(),
        tenant_id: tenant.clone(),
        updated_at: ts(2_200),
    };
    let conv_c = Conversation {
        id: ConversationId::parse(unique_entity("conv", "list-c")).unwrap(),
        account_id: other.id.clone(),
        tenant_id: tenant.clone(),
        updated_at: ts(2_300),
    };
    for conversation in [&conv_a, &conv_b, &conv_c] {
        store.upsert_conversation(conversation).await.unwrap();
    }

    let listed = store.list_conversations(&tenant, &owner.id).await.unwrap();
    // Most recently updated first, same account only.
    assert_eq!(listed, vec![conv_b.clone(), conv_a.clone()]);
}

#[tokio::test]
async fn messages_are_oldest_first_and_dedup_on_conflict() {
    let Some(store) = gated().await else {
        eprintln!("skip: {ENV} is not set (CI has no database)");
        return;
    };
    let tenant = unique_tenant("msgs");
    store
        .upsert_tenant(&tenant_record(&tenant, "Msgs"))
        .await
        .unwrap();
    let owner = account(
        &unique_entity("acc", "msgs-owner"),
        &tenant,
        AccountState::Online,
        None,
    );
    store.upsert_account(&owner).await.unwrap();
    let conversation = Conversation {
        id: ConversationId::parse(unique_entity("conv", "msgs-1")).unwrap(),
        account_id: owner.id.clone(),
        tenant_id: tenant.clone(),
        updated_at: ts(2_100),
    };
    store.upsert_conversation(&conversation).await.unwrap();

    let build = |id: &str, at: i64| {
        Message::new(
            fishpond_domain::MessageId::parse(id).unwrap(),
            conversation.id.clone(),
            MessageDirection::Outbound,
            format!("body of {id}"),
            ts(at),
        )
        .unwrap()
    };
    let first = build(&unique_entity("msg", "first"), 3_100);
    let second = build(&unique_entity("msg", "second"), 3_200);
    let third = build(&unique_entity("msg", "third"), 3_300);
    // Insert out of order; the repository must return them oldest first.
    for message in [&third, &first, &second] {
        assert!(
            store.insert_message(&tenant, message).await.unwrap(),
            "fresh insert reports inserted"
        );
    }
    let listed = store
        .list_messages(&tenant, &conversation.id)
        .await
        .unwrap();
    assert_eq!(
        listed,
        vec![first.clone(), second.clone(), third.clone()],
        "history must come back oldest first"
    );

    // Re-inserting the same id is a no-op that reports "deduplicated".
    assert!(
        !store.insert_message(&tenant, &first).await.unwrap(),
        "duplicate id must not insert again"
    );
    let listed = store
        .list_messages(&tenant, &conversation.id)
        .await
        .unwrap();
    assert_eq!(listed.len(), 3, "duplicate insert must not add a row");
}

#[tokio::test]
async fn products_roundtrip_scoped_by_tenant() {
    let Some(store) = gated().await else {
        eprintln!("skip: {ENV} is not set (CI has no database)");
        return;
    };
    let tenant_a = unique_tenant("prod-a");
    let tenant_b = unique_tenant("prod-b");
    store
        .upsert_tenant(&tenant_record(&tenant_a, "ProdA"))
        .await
        .unwrap();
    store
        .upsert_tenant(&tenant_record(&tenant_b, "ProdB"))
        .await
        .unwrap();

    let id_a = unique_entity("prod", "list-a");
    let product_a = Product::new(
        ProductId::parse(&id_a).unwrap(),
        tenant_a.clone(),
        "Keyboard",
        29_900,
        "CNY",
        ProductStatus::Listed,
        ts(1_500),
    )
    .unwrap();
    store.upsert_product(&product_a).await.unwrap();
    let product_b = Product::new(
        ProductId::parse(unique_entity("prod", "list-b")).unwrap(),
        tenant_b.clone(),
        "Mouse",
        9_900,
        "CNY",
        ProductStatus::Delisted,
        ts(1_600),
    )
    .unwrap();
    store.upsert_product(&product_b).await.unwrap();

    let listed_a = store.list_products(&tenant_a).await.unwrap();
    assert_eq!(listed_a, vec![product_a.clone()]);
    let listed_b = store.list_products(&tenant_b).await.unwrap();
    assert_eq!(listed_b, vec![product_b.clone()]);

    // Upsert updates in place.
    let renamed = Product::new(
        ProductId::parse(&id_a).unwrap(),
        tenant_a.clone(),
        "Keyboard Pro",
        39_900,
        "CNY",
        ProductStatus::Delisted,
        ts(2_500),
    )
    .unwrap();
    store.upsert_product(&renamed).await.unwrap();
    let listed_a = store.list_products(&tenant_a).await.unwrap();
    assert_eq!(listed_a, vec![renamed.clone()]);
}

#[tokio::test]
async fn unreachable_database_surfaces_a_database_error() {
    // No database needed: the connection itself must fail cleanly.
    let err = match Store::connect(
        "postgres://fishpond_test:fishpond_test@127.0.0.1:1/none?connect_timeout=1",
    )
    .await
    {
        Ok(_) => panic!("port 1 refuses connections, so connect must fail"),
        Err(e) => e,
    };
    assert!(matches!(err, StorageError::Database(_)), "got {err:?}");
}
