//! PostgreSQL store: schema migrations and tenant-scoped repositories.
//!
//! Every row carries the owning `tenant_id` and every repository method
//! runs inside a transaction bound to that tenant
//! ([`Store::begin_tenant`]), so isolation is enforced twice: by the
//! `WHERE tenant_id = $1` clause *and* by the row-level security policies
//! of migration 0002. Binding is transaction-local, so a pooled connection
//! can never leak a tenant to a later caller. Unbound sessions (raw pool
//! access) see no rows — fail closed; migration and operational work runs
//! as a superuser, which bypasses row-level security.
//!
//! Migrations embed at compile time via [`sqlx::migrate!`], so CI builds
//! without a database, and the runtime queries below avoid sqlx's
//! compile-time verification (also skipped without a database).

use fishpond_domain::{
    Account, AccountId, AccountState, Conversation, ConversationId, EventId, EventPayload, Message,
    MessageDirection, Product, ProductId, ProductStatus, TenantId, UnixMillis,
};
use sha2::{Digest, Sha256};
use sqlx::migrate::Migrate;
use sqlx::postgres::PgPoolOptions;
use sqlx::{PgPool, Postgres, Row, Transaction};

use crate::error::{StorageError, StorageResult};
use crate::events::{NewEvent, append_events_tx};

/// Migrations embedded at compile time from `crates/storage/migrations`.
static MIGRATIONS: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");

/// What deleting one account removed (the per-table cascade counts,
/// also carried in the `account.deleted` audit payload).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DeletedAccount {
    /// Conversation threads removed.
    pub conversations: u64,
    /// Chat messages removed (children of the conversations).
    pub messages: u64,
    /// Takeover grants removed.
    pub grants: u64,
    /// Authorization sessions removed.
    pub sessions: u64,
    /// Account leases removed.
    pub leases: u64,
    /// Browser profiles removed.
    pub profiles: u64,
}

/// A tenant row (the domain models tenant ids only).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TenantRecord {
    /// Tenant id.
    pub id: TenantId,
    /// Display name.
    pub name: String,
    /// Creation instant.
    pub created_at: UnixMillis,
}

/// Rows affected by one executed statement.
fn count(result: sqlx::postgres::PgQueryResult) -> u64 {
    result.rows_affected()
}

/// First 24 hex characters of a digest — enough entropy for a
/// tenant-scoped event id, short enough for the id length cap.
fn hex_head(digest: &[u8]) -> String {
    use std::fmt::Write;
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest {
        let _ = write!(hex, "{byte:02x}");
    }
    hex.truncate(24);
    hex
}

/// Owns the pool and the repository surface.
///
/// Cheap to clone (the pool is reference-counted): services hold their
/// own handle and share one set of connections.
#[derive(Clone)]
pub struct Store {
    pub(crate) pool: PgPool,
}

impl Store {
    /// Connects, creating a pool with a small connection limit and a
    /// bounded acquisition timeout, then runs the embedded migrations
    /// (idempotent: already-applied versions are skipped).
    pub async fn connect(url: &str) -> StorageResult<Self> {
        let store = Self::connect_unmigrated(url).await?;
        store.run_migrations().await?;
        Ok(store)
    }

    /// Connects without running migrations — for read-only health
    /// checks (doctor) that must not mutate the schema.
    pub async fn connect_unmigrated(url: &str) -> StorageResult<Self> {
        let pool = PgPoolOptions::new()
            .max_connections(5)
            .acquire_timeout(std::time::Duration::from_secs(5))
            .connect(url)
            .await?;
        Ok(Self { pool })
    }

    /// Runs pending migrations; safe to call repeatedly. Returns how
    /// many migrations this call applied.
    pub async fn run_migrations(&self) -> StorageResult<usize> {
        let before = self.applied_count().await?;
        MIGRATIONS.run(&self.pool).await?;
        let after = self.applied_count().await?;
        Ok(after.saturating_sub(before))
    }

    /// Round-trips the database (`SELECT 1`) so lazy pools prove
    /// connectivity.
    pub async fn ping(&self) -> StorageResult<()> {
        sqlx::query("SELECT 1").execute(&self.pool).await?;
        Ok(())
    }

    /// Whether every embedded migration is applied — true when nothing
    /// is pending.
    pub async fn schema_current(&self) -> StorageResult<bool> {
        let mut conn = self.pool.acquire().await?;
        let applied = conn.list_applied_migrations().await?;
        Ok(MIGRATIONS.iter().all(|embedded| {
            applied
                .iter()
                .any(|applied| applied.version == embedded.version)
        }))
    }

    /// How many migrations the `_sqlx_migrations` table records.
    async fn applied_count(&self) -> StorageResult<usize> {
        let mut conn = self.pool.acquire().await?;
        let applied = conn.list_applied_migrations().await?;
        Ok(applied.len())
    }

    /// The underlying pool.
    ///
    /// Raw pool access is **not** bound to any tenant, so under the RLS
    /// policies of migration 0002 it sees and writes nothing in the
    /// tenant-owned tables. Use it for schema/system-catalog queries, or
    /// prefer [`Store::begin_tenant`] for tenant data.
    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// Begins a transaction with its connection bound to `tenant`.
    ///
    /// Row-level security compares rows against `current_setting('app.tenant')`,
    /// so a bound transaction only ever sees — and may only write — the
    /// rows of its own tenant, even from unfiltered SQL. The binding is
    /// transaction-local (it dies with the transaction) and therefore safe
    /// with a pooled connection. Unbound sessions match nothing.
    pub async fn begin_tenant(
        &self,
        tenant: &TenantId,
    ) -> StorageResult<Transaction<'_, Postgres>> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("SELECT set_config('app.tenant', $1, TRUE)")
            .bind(tenant.as_str())
            .execute(&mut *tx)
            .await?;
        Ok(tx)
    }

    // ---- tenants ----

    /// Inserts or updates a tenant row.
    pub async fn upsert_tenant(&self, tenant: &TenantRecord) -> StorageResult<()> {
        let mut tx = self.begin_tenant(&tenant.id).await?;
        sqlx::query(
            "INSERT INTO tenants (id, name, created_at) VALUES ($1, $2, $3)
             ON CONFLICT (id) DO UPDATE SET name = EXCLUDED.name, created_at = EXCLUDED.created_at",
        )
        .bind(tenant.id.as_str())
        .bind(&tenant.name)
        .bind(tenant.created_at.as_millis())
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Fetches one tenant, or [`StorageError::NotFound`].
    pub async fn get_tenant(&self, id: &TenantId) -> StorageResult<TenantRecord> {
        let mut tx = self.begin_tenant(id).await?;
        let row = sqlx::query("SELECT id, name, created_at FROM tenants WHERE id = $1")
            .bind(id.as_str())
            .fetch_optional(&mut *tx)
            .await?;
        tx.commit().await?;
        let row = row.ok_or_else(|| StorageError::NotFound {
            what: "tenant",
            id: id.as_str().to_owned(),
        })?;
        Ok(TenantRecord {
            id: TenantId::parse(row.try_get::<String, _>("id")?)?,
            name: row.try_get::<String, _>("name")?,
            created_at: UnixMillis::from_millis(row.try_get::<i64, _>("created_at")?)?,
        })
    }

    // ---- accounts ----

    /// Inserts or updates an account row.
    pub async fn upsert_account(&self, account: &Account) -> StorageResult<()> {
        let mut tx = self.begin_tenant(&account.tenant_id).await?;
        sqlx::query(
            "INSERT INTO accounts (id, tenant_id, platform, state, state_note, updated_at)
             VALUES ($1, $2, $3, $4, $5, $6)
             ON CONFLICT (id) DO UPDATE
               SET platform = EXCLUDED.platform,
                   state = EXCLUDED.state,
                   state_note = EXCLUDED.state_note,
                   updated_at = EXCLUDED.updated_at",
        )
        .bind(account.id.as_str())
        .bind(account.tenant_id.as_str())
        .bind(&account.platform)
        .bind(account.state.as_str())
        .bind(&account.state_note)
        .bind(account.updated_at.as_millis())
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Fetches one account **of `tenant`**, or [`StorageError::NotFound`].
    /// The tenant parameter is mandatory: an account id alone names a row
    /// another tenant may own, which row-level security refuses to return.
    pub async fn get_account(&self, tenant: &TenantId, id: &AccountId) -> StorageResult<Account> {
        let mut tx = self.begin_tenant(tenant).await?;
        let row = sqlx::query(
            "SELECT id, tenant_id, platform, state, state_note, updated_at
             FROM accounts WHERE id = $1",
        )
        .bind(id.as_str())
        .fetch_optional(&mut *tx)
        .await?;
        tx.commit().await?;
        let row = row.ok_or_else(|| StorageError::NotFound {
            what: "account",
            id: id.as_str().to_owned(),
        })?;
        map_account(&row)
    }

    /// Lists a tenant's accounts, ordered by id.
    pub async fn list_accounts(&self, tenant: &TenantId) -> StorageResult<Vec<Account>> {
        let mut tx = self.begin_tenant(tenant).await?;
        let rows = sqlx::query(
            "SELECT id, tenant_id, platform, state, state_note, updated_at
             FROM accounts WHERE tenant_id = $1 ORDER BY id",
        )
        .bind(tenant.as_str())
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;
        rows.iter().map(map_account).collect()
    }

    /// Updates an account's state/note for `tenant` and returns the fresh
    /// row, or [`StorageError::NotFound`] when the account does not belong
    /// to that tenant (the update affects no row and the re-read sees
    /// nothing, by policy).
    pub async fn update_account_state(
        &self,
        tenant: &TenantId,
        id: &AccountId,
        state: AccountState,
        state_note: Option<String>,
        at: UnixMillis,
    ) -> StorageResult<Account> {
        let mut tx = self.begin_tenant(tenant).await?;
        sqlx::query(
            "UPDATE accounts SET state = $2, state_note = $3, updated_at = $4 WHERE id = $1",
        )
        .bind(id.as_str())
        .bind(state.as_str())
        .bind(state_note)
        .bind(at.as_millis())
        .execute(&mut *tx)
        .await?;
        let row = sqlx::query(
            "SELECT id, tenant_id, platform, state, state_note, updated_at
             FROM accounts WHERE id = $1",
        )
        .bind(id.as_str())
        .fetch_optional(&mut *tx)
        .await?;
        tx.commit().await?;
        let row = row.ok_or_else(|| StorageError::NotFound {
            what: "account",
            id: id.as_str().to_owned(),
        })?;
        map_account(&row)
    }

    /// Deletes one account **of `tenant`** and every hard foreign-key
    /// child in a single transaction: conversations and their messages,
    /// takeover grants, auth sessions, account leases and profiles.
    /// Observation history survives (migration 0014 dropped that FK) —
    /// it is the human-readable audit trail; the `account.deleted`
    /// event appended atomically with the deletion is the
    /// machine-readable one and carries the per-table counts.
    ///
    /// Returns [`StorageError::NotFound`] when the account does not
    /// belong to the tenant.
    pub async fn delete_account(
        &self,
        tenant: &TenantId,
        id: &AccountId,
        at: UnixMillis,
    ) -> StorageResult<DeletedAccount> {
        let mut tx = self.begin_tenant(tenant).await?;
        let row = sqlx::query("SELECT platform FROM accounts WHERE id = $1")
            .bind(id.as_str())
            .fetch_optional(&mut *tx)
            .await?;
        let platform: String = row
            .ok_or_else(|| StorageError::NotFound {
                what: "account",
                id: id.as_str().to_owned(),
            })?
            .try_get("platform")?;

        let messages = count(
            sqlx::query(
                "DELETE FROM messages WHERE tenant_id = $1 AND conversation_id IN
                 (SELECT id FROM conversations WHERE tenant_id = $1 AND account_id = $2)",
            )
            .bind(tenant.as_str())
            .bind(id.as_str())
            .execute(&mut *tx)
            .await?,
        );
        let conversations = count(
            sqlx::query("DELETE FROM conversations WHERE tenant_id = $1 AND account_id = $2")
                .bind(tenant.as_str())
                .bind(id.as_str())
                .execute(&mut *tx)
                .await?,
        );
        let grants = count(
            sqlx::query("DELETE FROM takeover_grants WHERE tenant_id = $1 AND account_id = $2")
                .bind(tenant.as_str())
                .bind(id.as_str())
                .execute(&mut *tx)
                .await?,
        );
        let sessions = count(
            sqlx::query("DELETE FROM auth_sessions WHERE tenant_id = $1 AND account_id = $2")
                .bind(tenant.as_str())
                .bind(id.as_str())
                .execute(&mut *tx)
                .await?,
        );
        let leases = count(
            sqlx::query("DELETE FROM account_leases WHERE tenant_id = $1 AND account_id = $2")
                .bind(tenant.as_str())
                .bind(id.as_str())
                .execute(&mut *tx)
                .await?,
        );
        let profiles = count(
            sqlx::query("DELETE FROM profiles WHERE tenant_id = $1 AND account_id = $2")
                .bind(tenant.as_str())
                .bind(id.as_str())
                .execute(&mut *tx)
                .await?,
        );
        let deleted = DeletedAccount {
            conversations,
            messages,
            sessions,
            grants,
            leases,
            profiles,
        };
        sqlx::query("DELETE FROM accounts WHERE id = $1")
            .bind(id.as_str())
            .execute(&mut *tx)
            .await?;

        // The audit event id is a digest of (account, deletion instant):
        // deterministic, unique per deletion, and safe even if an id is
        // re-registered and deleted again later.
        let digest = Sha256::digest(format!("{}-{}", id.as_str(), at.as_millis()));
        let event_id = EventId::parse(format!("evt-del-{}", hex_head(&digest))).map_err(|err| {
            StorageError::Invalid {
                what: "account.deleted event id",
                reason: err.to_string(),
            }
        })?;
        let payload = EventPayload::parse(
            serde_json::json!({
                "account": id.as_str(),
                "platform": platform,
                "conversations": deleted.conversations,
                "messages": deleted.messages,
                "sessions": deleted.sessions,
                "grants": deleted.grants,
                "leases": deleted.leases,
                "profiles": deleted.profiles,
            })
            .to_string(),
        )
        .map_err(|err| StorageError::Invalid {
            what: "account.deleted payload",
            reason: err.to_string(),
        })?;
        append_events_tx(
            &mut tx,
            tenant,
            &[NewEvent {
                id: &event_id,
                event_type: "account.deleted",
                account_id: Some(id),
                occurred_at: at,
                payload: &payload,
            }],
        )
        .await?;
        tx.commit().await?;
        Ok(deleted)
    }

    // ---- conversations ----

    /// Inserts or updates a conversation row.
    pub async fn upsert_conversation(&self, conversation: &Conversation) -> StorageResult<()> {
        let mut tx = self.begin_tenant(&conversation.tenant_id).await?;
        sqlx::query(
            "INSERT INTO conversations (id, account_id, tenant_id, updated_at)
             VALUES ($1, $2, $3, $4)
             ON CONFLICT (id) DO UPDATE
               SET account_id = EXCLUDED.account_id,
                   tenant_id = EXCLUDED.tenant_id,
                   updated_at = EXCLUDED.updated_at",
        )
        .bind(conversation.id.as_str())
        .bind(conversation.account_id.as_str())
        .bind(conversation.tenant_id.as_str())
        .bind(conversation.updated_at.as_millis())
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Fetches one conversation **of `tenant`**, or
    /// [`StorageError::NotFound`].
    pub async fn get_conversation(
        &self,
        tenant: &TenantId,
        id: &ConversationId,
    ) -> StorageResult<Conversation> {
        let mut tx = self.begin_tenant(tenant).await?;
        let row = sqlx::query(
            "SELECT id, account_id, tenant_id, updated_at
             FROM conversations WHERE id = $1",
        )
        .bind(id.as_str())
        .fetch_optional(&mut *tx)
        .await?;
        tx.commit().await?;
        let row = row.ok_or_else(|| StorageError::NotFound {
            what: "conversation",
            id: id.as_str().to_owned(),
        })?;
        map_conversation(&row)
    }

    /// Lists a tenant+account's conversations, most recently updated first.
    pub async fn list_conversations(
        &self,
        tenant: &TenantId,
        account: &AccountId,
    ) -> StorageResult<Vec<Conversation>> {
        let mut tx = self.begin_tenant(tenant).await?;
        let rows = sqlx::query(
            "SELECT id, account_id, tenant_id, updated_at
             FROM conversations
             WHERE tenant_id = $1 AND account_id = $2
             ORDER BY updated_at DESC, id",
        )
        .bind(tenant.as_str())
        .bind(account.as_str())
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;
        rows.iter().map(map_conversation).collect()
    }

    // ---- messages ----

    /// Inserts a message for `tenant` (the domain [`Message`] carries no
    /// tenant of its own); returns whether the insert actually happened —
    /// false means the id was already stored, i.e. a duplicate was
    /// suppressed.
    pub async fn insert_message(
        &self,
        tenant: &TenantId,
        message: &Message,
    ) -> StorageResult<bool> {
        let mut tx = self.begin_tenant(tenant).await?;
        let row = sqlx::query(
            "INSERT INTO messages (id, conversation_id, tenant_id, direction, text, sent_at)
             VALUES ($1, $2, $3, $4, $5, $6)
             ON CONFLICT (id) DO NOTHING
             RETURNING id",
        )
        .bind(message.id.as_str())
        .bind(message.conversation_id.as_str())
        .bind(tenant.as_str())
        .bind(message.direction.as_str())
        .bind(&message.text)
        .bind(message.sent_at.as_millis())
        .fetch_optional(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(row.is_some())
    }

    /// Lists one conversation's messages, oldest first.
    pub async fn list_messages(
        &self,
        tenant: &TenantId,
        conversation: &ConversationId,
    ) -> StorageResult<Vec<Message>> {
        let mut tx = self.begin_tenant(tenant).await?;
        let rows = sqlx::query(
            "SELECT id, conversation_id, tenant_id, direction, text, sent_at
             FROM messages
             WHERE tenant_id = $1 AND conversation_id = $2
             ORDER BY sent_at ASC, id",
        )
        .bind(tenant.as_str())
        .bind(conversation.as_str())
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;
        rows.iter().map(map_message).collect()
    }

    // ---- products ----

    /// Inserts or updates a product row.
    pub async fn upsert_product(&self, product: &Product) -> StorageResult<()> {
        let mut tx = self.begin_tenant(&product.tenant_id).await?;
        sqlx::query(
            "INSERT INTO products (id, tenant_id, title, price_cents, currency, status, updated_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7)
             ON CONFLICT (id) DO UPDATE
               SET title = EXCLUDED.title,
                   price_cents = EXCLUDED.price_cents,
                   currency = EXCLUDED.currency,
                   status = EXCLUDED.status,
                   updated_at = EXCLUDED.updated_at",
        )
        .bind(product.id.as_str())
        .bind(product.tenant_id.as_str())
        .bind(&product.title)
        .bind(product.price_cents)
        .bind(&product.currency)
        .bind(product.status.as_str())
        .bind(product.updated_at.as_millis())
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Lists a tenant's products, ordered by id.
    pub async fn list_products(&self, tenant: &TenantId) -> StorageResult<Vec<Product>> {
        let mut tx = self.begin_tenant(tenant).await?;
        let rows = sqlx::query(
            "SELECT id, tenant_id, title, price_cents, currency, status, updated_at
             FROM products WHERE tenant_id = $1 ORDER BY id",
        )
        .bind(tenant.as_str())
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;
        rows.iter().map(map_product).collect()
    }
}

/// Maps an account row back into the validated domain type (stored rows
/// were validated on write; an inconsistent row surfaces as a domain error).
fn map_account(row: &sqlx::postgres::PgRow) -> StorageResult<Account> {
    let note: Option<String> = row.try_get("state_note")?;
    Ok(Account::new(
        AccountId::parse(row.try_get::<String, _>("id")?)?,
        TenantId::parse(row.try_get::<String, _>("tenant_id")?)?,
        row.try_get::<String, _>("platform")?,
        AccountState::parse(&row.try_get::<String, _>("state")?)?,
        note,
        UnixMillis::from_millis(row.try_get::<i64, _>("updated_at")?)?,
    )?)
}

/// Maps a conversation row back into the domain type.
fn map_conversation(row: &sqlx::postgres::PgRow) -> StorageResult<Conversation> {
    Ok(Conversation {
        id: ConversationId::parse(row.try_get::<String, _>("id")?)?,
        account_id: AccountId::parse(row.try_get::<String, _>("account_id")?)?,
        tenant_id: TenantId::parse(row.try_get::<String, _>("tenant_id")?)?,
        updated_at: UnixMillis::from_millis(row.try_get::<i64, _>("updated_at")?)?,
    })
}

/// Maps a message row back into the validated domain type.
fn map_message(row: &sqlx::postgres::PgRow) -> StorageResult<Message> {
    let direction: String = row.try_get("direction")?;
    let direction = match direction.as_str() {
        "inbound" => MessageDirection::Inbound,
        "outbound" => MessageDirection::Outbound,
        other => {
            return Err(StorageError::Domain(
                fishpond_domain::DomainError::InvalidStateName {
                    machine: "message direction",
                    raw: other.to_owned(),
                },
            ));
        }
    };
    Ok(Message::new(
        fishpond_domain::MessageId::parse(row.try_get::<String, _>("id")?)?,
        ConversationId::parse(row.try_get::<String, _>("conversation_id")?)?,
        direction,
        row.try_get::<String, _>("text")?,
        UnixMillis::from_millis(row.try_get::<i64, _>("sent_at")?)?,
    )?)
}

/// Maps a product row back into the validated domain type.
fn map_product(row: &sqlx::postgres::PgRow) -> StorageResult<Product> {
    let status: String = row.try_get("status")?;
    let status = match status.as_str() {
        "listed" => ProductStatus::Listed,
        "delisted" => ProductStatus::Delisted,
        "removed" => ProductStatus::Removed,
        other => {
            return Err(StorageError::Domain(
                fishpond_domain::DomainError::InvalidStateName {
                    machine: "product status",
                    raw: other.to_owned(),
                },
            ));
        }
    };
    Ok(Product::new(
        ProductId::parse(row.try_get::<String, _>("id")?)?,
        TenantId::parse(row.try_get::<String, _>("tenant_id")?)?,
        row.try_get::<String, _>("title")?,
        row.try_get::<i64, _>("price_cents")?,
        row.try_get::<String, _>("currency")?,
        status,
        UnixMillis::from_millis(row.try_get::<i64, _>("updated_at")?)?,
    )?)
}
