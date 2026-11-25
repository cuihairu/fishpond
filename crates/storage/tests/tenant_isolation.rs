//! F007 acceptance: API keys, RBAC scopes and cross-tenant isolation under
//! row-level security.
//!
//! Runs against the same env-gated database as the repository tests (see
//! `common`). The isolation checks deliberately issue *unfiltered* SQL
//! inside tenant-bound transactions: with the `WHERE` clauses gone, only
//! the database policies keep the tenants apart. The test role is a
//! non-superuser, because superusers bypass row-level security and would
//! make every assertion vacuously pass.

mod common;

use common::*;

use fishpond_domain::{
    AccountState, Conversation, ConversationId, Message, MessageDirection, Product, ProductId,
    ProductStatus,
};
use fishpond_storage::{
    SCOPE_ACCOUNTS_READ, SCOPE_ALL, SCOPE_CATALOG_READ, SCOPE_CONVERSATIONS_READ,
    SCOPE_CONVERSATIONS_WRITE, SCOPE_EVENTS_READ, SCOPE_OPERATIONS_READ, StorageError,
    hash_api_key, require_scope,
};
use sqlx::Executor;
use sqlx::Row;

#[tokio::test]
async fn seeded_roles_match_the_scope_vocabulary() {
    let Some(store) = gated().await else {
        eprintln!("skip: {ENV} is not set (CI has no database)");
        return;
    };

    let roles = store.list_roles().await.expect("list roles");
    // Earlier runs may have seeded custom roles too; the migration's own
    // vocabulary must simply always be present.
    for expected in ["owner", "operator", "viewer"] {
        assert!(
            roles.iter().any(|r| r.name == expected),
            "migration 0002 must seed the {expected} role"
        );
    }
    let owner = roles
        .iter()
        .find(|r| r.name == "owner")
        .expect("owner role");
    assert_eq!(owner.scopes, SCOPE_ALL);
    let operator = roles
        .iter()
        .find(|r| r.name == "operator")
        .expect("operator role");
    assert_eq!(operator.scopes, SCOPE_ALL);
    let viewer = roles
        .iter()
        .find(|r| r.name == "viewer")
        .expect("viewer role");
    assert!(
        !viewer.scopes.iter().any(|s| s == SCOPE_CONVERSATIONS_WRITE),
        "viewer must not be able to write"
    );
    assert_eq!(viewer.scopes.len(), 5);

    // Roles patch idempotently.
    store
        .upsert_role("owner", &[SCOPE_ACCOUNTS_READ])
        .await
        .expect("patch owner");
    let patched = store
        .list_roles()
        .await
        .expect("list roles again")
        .into_iter()
        .find(|r| r.name == "owner")
        .expect("owner role still there");
    assert_eq!(patched.scopes, vec![SCOPE_ACCOUNTS_READ.to_owned()]);

    // Restore the seeded vocabulary for later runs.
    store
        .upsert_role("owner", &SCOPE_ALL)
        .await
        .expect("restore owner");
    let restored = store
        .list_roles()
        .await
        .expect("list roles once more")
        .into_iter()
        .find(|r| r.name == "owner")
        .expect("owner role present");
    assert_eq!(restored.scopes, SCOPE_ALL);
}

#[tokio::test]
async fn api_keys_freeze_role_scopes_and_enforce_permissions() {
    let Some(store) = gated().await else {
        eprintln!("skip: {ENV} is not set (CI has no database)");
        return;
    };
    let tenant = unique_tenant("keys");
    store
        .upsert_tenant(&tenant_record(&tenant, "Keys"))
        .await
        .expect("tenant insert");

    let owner_key = format!("fpk-{}", unique_token("owner"));
    let viewer_key = format!("fpk-{}", unique_token("viewer"));

    let issued = store
        .issue_api_key(&owner_key, &tenant, "owner", ts(0), ts(20))
        .await
        .expect("issue owner key");
    assert_eq!(issued.key_hash, hash_api_key(&owner_key));
    assert_eq!(issued.tenant_id, tenant);
    assert_eq!(issued.role, "owner");
    assert_eq!(issued.scopes, SCOPE_ALL);
    assert!(!issued.is_revoked);
    assert_eq!(issued.key_hint.len(), 4);
    // Only the tail is kept; the raw key is never stored.
    assert!(!format!("{issued:?}").contains(&owner_key));

    store
        .issue_api_key(&viewer_key, &tenant, "viewer", ts(0), ts(20))
        .await
        .expect("issue viewer key");

    // Resolution round-trips the frozen scopes.
    let Some(resolved) = store
        .resolve_api_key(&owner_key, ts(10))
        .await
        .expect("resolve owner key")
    else {
        panic!("issued owner key must resolve")
    };
    assert_eq!(resolved.tenant_id, tenant);
    assert_eq!(resolved.role, "owner");
    assert!(resolved.has_scope(SCOPE_ACCOUNTS_READ));

    // The viewer may read but not write: the store enforces the scope.
    let Some(viewer) = store
        .resolve_api_key(&viewer_key, ts(10))
        .await
        .expect("resolve viewer key")
    else {
        panic!("issued viewer key must resolve")
    };
    assert!(!viewer.has_scope(SCOPE_CONVERSATIONS_WRITE));
    let denied =
        require_scope(&viewer, SCOPE_CONVERSATIONS_WRITE).expect_err("viewer write must be denied");
    let denied_text = denied.to_string();
    let StorageError::PermissionDenied {
        tenant: denied_tenant,
        role,
        scope,
    } = denied
    else {
        panic!("expected PermissionDenied, got {denied_text}")
    };
    assert_eq!(denied_tenant, tenant.as_str());
    assert_eq!(role, "viewer");
    assert_eq!(scope, SCOPE_CONVERSATIONS_WRITE);
    // Neither the error nor any record echoes the raw key material.
    assert!(!denied_text.contains(&viewer_key));
    assert!(
        !store
            .list_api_keys(&tenant)
            .await
            .expect("list keys")
            .iter()
            .any(|k| format!("{k:?}").contains(&viewer_key)),
        "stored records must not carry raw key material"
    );
    require_scope(&viewer, SCOPE_ACCOUNTS_READ).expect("viewer read is allowed");

    // Frozen at issue time: widening the role never widens a live key.
    store
        .upsert_role(
            "viewer",
            &[
                SCOPE_ACCOUNTS_READ,
                SCOPE_CONVERSATIONS_READ,
                SCOPE_CATALOG_READ,
                SCOPE_OPERATIONS_READ,
                SCOPE_EVENTS_READ,
                SCOPE_CONVERSATIONS_WRITE,
            ],
        )
        .await
        .expect("widen viewer");
    let widened = store
        .resolve_api_key(&viewer_key, ts(10))
        .await
        .expect("resolve widened viewer")
        .expect("viewer key still resolves");
    assert!(
        !widened.has_scope(SCOPE_CONVERSATIONS_WRITE),
        "scopes are frozen at issue time"
    );
    // Restore the seeded scope set for later runs.
    store
        .upsert_role(
            "viewer",
            &[
                SCOPE_ACCOUNTS_READ,
                SCOPE_CONVERSATIONS_READ,
                SCOPE_CATALOG_READ,
                SCOPE_OPERATIONS_READ,
                SCOPE_EVENTS_READ,
            ],
        )
        .await
        .expect("restore viewer");

    // A custom role can be seeded and issued like the built-ins.
    let auditor_role = format!("auditor-{}", unique_token("r"));
    store
        .upsert_role(&auditor_role, &[SCOPE_EVENTS_READ])
        .await
        .expect("seed custom role");
    let auditor_key = format!("fpk-{}", unique_token("aud"));
    store
        .issue_api_key(&auditor_key, &tenant, &auditor_role, ts(0), ts(20))
        .await
        .expect("issue auditor key");
    let resolved_auditor = store
        .resolve_api_key(&auditor_key, ts(1))
        .await
        .expect("resolve auditor key")
        .expect("auditor key resolves");
    assert_eq!(resolved_auditor.role, auditor_role);
    assert_eq!(resolved_auditor.scopes, vec![SCOPE_EVENTS_READ.to_owned()]);
}

#[tokio::test]
async fn revoked_expired_and_unknown_keys_do_not_resolve() {
    let Some(store) = gated().await else {
        eprintln!("skip: {ENV} is not set (CI has no database)");
        return;
    };
    let tenant = unique_tenant("keys-dead");
    store
        .upsert_tenant(&tenant_record(&tenant, "Dead Keys"))
        .await
        .expect("tenant insert");

    let live = format!("fpk-{}", unique_token("live"));
    let dead = format!("fpk-{}", unique_token("dead"));
    let expired = format!("fpk-{}", unique_token("expired"));

    store
        .issue_api_key(&live, &tenant, "viewer", ts(0), ts(20))
        .await
        .expect("issue live");
    let issued = store
        .issue_api_key(&dead, &tenant, "viewer", ts(0), ts(20))
        .await
        .expect("issue dead");
    store
        .issue_api_key(&expired, &tenant, "viewer", ts(0), ts(5))
        .await
        .expect("issue expired");

    assert!(store.revoke_api_key(&issued.key_hash, ts(1)).await.unwrap());
    // Revocation is idempotent: the second call revokes nothing.
    assert!(
        !store.revoke_api_key(&issued.key_hash, ts(2)).await.unwrap(),
        "revoking twice must report nothing newly revoked"
    );

    assert!(
        store
            .resolve_api_key(&live, ts(10))
            .await
            .unwrap()
            .is_some()
    );
    assert!(
        store
            .resolve_api_key(&dead, ts(10))
            .await
            .unwrap()
            .is_none(),
        "revoked keys must not resolve"
    );
    assert!(
        store
            .resolve_api_key(&expired, ts(6))
            .await
            .unwrap()
            .is_none(),
        "keys stop resolving at or after expiry"
    );
    assert!(
        store
            .resolve_api_key(&format!("fpk-{}", unique_token("unknown")), ts(1))
            .await
            .unwrap()
            .is_none(),
        "unknown keys must not resolve"
    );
    assert!(
        store.resolve_api_key("", ts(1)).await.unwrap().is_none(),
        "blank keys must not resolve"
    );

    let blank = store
        .issue_api_key("   ", &tenant, "viewer", ts(0), ts(10))
        .await
        .expect_err("blank keys must be refused");
    assert!(matches!(blank, StorageError::Invalid { .. }));

    let listed = store.list_api_keys(&tenant).await.unwrap();
    assert_eq!(listed.len(), 3);
    let dead_record = listed
        .iter()
        .find(|k| k.key_hash == issued.key_hash)
        .expect("dead key is listed");
    assert!(dead_record.is_revoked);
    assert_eq!(dead_record.revoked_at, Some(ts(1)));
    assert_eq!(dead_record.key_hash, hash_api_key(&dead));
    assert!(!listed.iter().any(|k| format!("{k:?}").contains(&dead)));
}

#[tokio::test]
async fn bound_sessions_see_only_their_tenant() {
    let Some(store) = gated().await else {
        eprintln!("skip: {ENV} is not set (CI has no database)");
        return;
    };
    let tenant_a = unique_tenant("rls-a");
    let tenant_b = unique_tenant("rls-b");
    store
        .upsert_tenant(&tenant_record(&tenant_a, "A"))
        .await
        .unwrap();
    store
        .upsert_tenant(&tenant_record(&tenant_b, "B"))
        .await
        .unwrap();

    let acc_a = account(
        &unique_entity("acc", "rls-a"),
        &tenant_a,
        AccountState::Online,
        None,
    );
    let acc_b = account(
        &unique_entity("acc", "rls-b"),
        &tenant_b,
        AccountState::Online,
        None,
    );
    store.upsert_account(&acc_a).await.unwrap();
    store.upsert_account(&acc_b).await.unwrap();

    let conv_a = Conversation {
        id: ConversationId::parse(unique_entity("conv", "rls-a")).unwrap(),
        account_id: acc_a.id.clone(),
        tenant_id: tenant_a.clone(),
        updated_at: ts(2_500),
    };
    let conv_b = Conversation {
        id: ConversationId::parse(unique_entity("conv", "rls-b")).unwrap(),
        account_id: acc_b.id.clone(),
        tenant_id: tenant_b.clone(),
        updated_at: ts(2_600),
    };
    store.upsert_conversation(&conv_a).await.unwrap();
    store.upsert_conversation(&conv_b).await.unwrap();

    let msg = |conversation: &ConversationId, at: i64| {
        Message::new(
            fishpond_domain::MessageId::parse(unique_entity("msg", "rls")).unwrap(),
            conversation.clone(),
            MessageDirection::Outbound,
            "body".to_owned(),
            ts(at),
        )
        .expect("valid message")
    };
    store
        .insert_message(&tenant_a, &msg(&conv_a.id, 2_700))
        .await
        .unwrap();
    store
        .insert_message(&tenant_b, &msg(&conv_b.id, 2_800))
        .await
        .unwrap();

    let prod_a = Product::new(
        ProductId::parse(unique_entity("prod", "rls-a")).unwrap(),
        tenant_a.clone(),
        "Keyboard",
        1_000,
        "CNY",
        ProductStatus::Listed,
        ts(2_400),
    )
    .unwrap();
    let prod_b = Product::new(
        ProductId::parse(unique_entity("prod", "rls-b")).unwrap(),
        tenant_b.clone(),
        "Mouse",
        2_000,
        "CNY",
        ProductStatus::Listed,
        ts(2_450),
    )
    .unwrap();
    store.upsert_product(&prod_a).await.unwrap();
    store.upsert_product(&prod_b).await.unwrap();

    // Unfiltered SQL inside a tenant-bound transaction: the database, not
    // the query shape, does the filtering.
    let mut tx_a = store.begin_tenant(&tenant_a).await.unwrap();
    let rows = sqlx::query_scalar::<_, String>("SELECT id FROM accounts ORDER BY id")
        .fetch_all(&mut *tx_a)
        .await
        .unwrap();
    assert_eq!(rows, vec![acc_a.id.as_str().to_owned()]);
    let rows = sqlx::query_scalar::<_, String>("SELECT id FROM tenants ORDER BY id")
        .fetch_all(&mut *tx_a)
        .await
        .unwrap();
    assert_eq!(rows, vec![tenant_a.as_str().to_owned()]);
    let rows = sqlx::query_scalar::<_, String>("SELECT id FROM conversations ORDER BY id")
        .fetch_all(&mut *tx_a)
        .await
        .unwrap();
    assert_eq!(rows, vec![conv_a.id.as_str().to_owned()]);
    let rows = sqlx::query_scalar::<_, String>("SELECT id FROM messages ORDER BY id")
        .fetch_all(&mut *tx_a)
        .await
        .unwrap();
    assert_eq!(
        rows.len(),
        1,
        "B's message must be invisible even unfiltered"
    );
    let rows = sqlx::query_scalar::<_, String>("SELECT id FROM products ORDER BY id")
        .fetch_all(&mut *tx_a)
        .await
        .unwrap();
    assert_eq!(rows, vec![prod_a.id.as_str().to_owned()]);
    tx_a.commit().await.unwrap();

    let mut tx_b = store.begin_tenant(&tenant_b).await.unwrap();
    let rows = sqlx::query_scalar::<_, String>("SELECT id FROM accounts ORDER BY id")
        .fetch_all(&mut *tx_b)
        .await
        .unwrap();
    assert_eq!(rows, vec![acc_b.id.as_str().to_owned()]);
    let rows = sqlx::query_scalar::<_, String>("SELECT id FROM conversations ORDER BY id")
        .fetch_all(&mut *tx_b)
        .await
        .unwrap();
    assert_eq!(rows, vec![conv_b.id.as_str().to_owned()]);
    tx_b.commit().await.unwrap();

    // Unbound pool access fails closed, on every tenant-owned table.
    for table in [
        "accounts",
        "conversations",
        "messages",
        "products",
        "tenants",
    ] {
        let rows = sqlx::query_scalar::<_, String>(&format!("SELECT id FROM {table} ORDER BY id"))
            .fetch_all(store.pool())
            .await
            .unwrap();
        assert!(
            rows.is_empty(),
            "unbound pool access must see no rows, got {rows:?}"
        );
    }

    // Unbound writes are refused too (WITH CHECK).
    let insert =
        sqlx::query("INSERT INTO tenants (id, name, created_at) VALUES ($1, 'unbound', 1)")
            .bind(unique_entity("t", "unbound"));
    let err = store
        .pool()
        .execute(insert)
        .await
        .expect_err("unbound insert must be refused");
    assert!(matches!(err, sqlx::Error::Database(_)));
}

#[tokio::test]
async fn bound_sessions_cannot_cross_write() {
    let Some(store) = gated().await else {
        eprintln!("skip: {ENV} is not set (CI has no database)");
        return;
    };
    let tenant_a = unique_tenant("xw-a");
    let tenant_b = unique_tenant("xw-b");
    store
        .upsert_tenant(&tenant_record(&tenant_a, "A"))
        .await
        .unwrap();
    store
        .upsert_tenant(&tenant_record(&tenant_b, "B"))
        .await
        .unwrap();
    let acc_a = account(
        &unique_entity("acc", "xw-a"),
        &tenant_a,
        AccountState::Online,
        None,
    );
    let acc_b = account(
        &unique_entity("acc", "xw-b"),
        &tenant_b,
        AccountState::Online,
        None,
    );
    store.upsert_account(&acc_a).await.unwrap();
    store.upsert_account(&acc_b).await.unwrap();

    // An unfiltered update bound to A touches only A's rows.
    let mut tx = store.begin_tenant(&tenant_a).await.unwrap();
    let affected = sqlx::query("UPDATE accounts SET platform = 'hacked'")
        .execute(&mut *tx)
        .await
        .unwrap()
        .rows_affected();
    assert_eq!(affected, 1, "only the bound tenant's row is visible");
    let after = sqlx::query("SELECT platform FROM accounts WHERE id = $1")
        .bind(acc_a.id.as_str())
        .fetch_one(&mut *tx)
        .await
        .unwrap();
    assert_eq!(
        after
            .try_get::<String, _>("platform")
            .expect("read platform back"),
        "hacked"
    );
    tx.commit().await.unwrap();

    assert_eq!(
        store
            .get_account(&tenant_b, &acc_b.id)
            .await
            .unwrap()
            .platform,
        "mock",
        "B's account must be untouched"
    );

    // An unfiltered insert bound to A cannot smuggle B's tenant. The check
    // runs in its own transaction because a refused statement aborts the
    // surrounding one.
    let mut tx = store.begin_tenant(&tenant_a).await.unwrap();
    let err = sqlx::query(
        "INSERT INTO accounts (id, tenant_id, platform, state, state_note, updated_at)
         VALUES ($1, $2, 'mock', 'online', NULL, 1)",
    )
    .bind(unique_entity("acc", "smuggle"))
    .bind(tenant_b.as_str())
    .execute(&mut *tx)
    .await
    .expect_err("cross-tenant write must be refused by WITH CHECK");
    assert!(matches!(err, sqlx::Error::Database(_)));
    tx.rollback().await.unwrap();

    let mut tx = store.begin_tenant(&tenant_a).await.unwrap();
    let visible = sqlx::query_scalar::<_, String>("SELECT id FROM accounts ORDER BY id")
        .fetch_all(&mut *tx)
        .await
        .unwrap();
    assert_eq!(visible, vec![acc_a.id.as_str().to_owned()]);
    tx.rollback().await.unwrap();

    // Unfiltered delete bound to A removes only A's rows.
    let mut tx = store.begin_tenant(&tenant_a).await.unwrap();
    let removed = sqlx::query("DELETE FROM accounts")
        .execute(&mut *tx)
        .await
        .unwrap()
        .rows_affected();
    assert_eq!(removed, 1);
    tx.commit().await.unwrap();
    let listed = store.list_accounts(&tenant_b).await.unwrap();
    assert_eq!(
        listed,
        vec![acc_b.clone()],
        "B must still see its own account"
    );
}
