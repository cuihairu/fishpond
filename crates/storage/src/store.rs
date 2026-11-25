//! PostgreSQL store: schema migrations and tenant-scoped repositories.
//!
//! All rows carry the owning `tenant_id`; every list/read query is scoped
//! by it so tenant isolation is enforced by the query shape itself (RLS
//! policies layered on top in F007). Migrations are embedded at compile
//! time via [`sqlx::migrate!`] so CI builds without a database, and the
//! same runtime queries avoid sqlx's compile-time verification (also
//! skipped when no database is available at build time).

use fishpond_domain::{
    Account, AccountId, AccountState, Conversation, ConversationId, Message, MessageDirection,
    Product, ProductId, ProductStatus, TenantId, UnixMillis,
};
use sqlx::postgres::PgPoolOptions;
use sqlx::{PgPool, Row};

use crate::error::{StorageError, StorageResult};

/// Migrations embedded at compile time from `crates/storage/migrations`.
static MIGRATIONS: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");

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

/// Owns the pool and the repository surface.
pub struct Store {
    pool: PgPool,
}

impl Store {
    /// Connects, creating a pool with a small connection limit and a
    /// bounded acquisition timeout, then runs the embedded migrations
    /// (idempotent: already-applied versions are skipped).
    pub async fn connect(url: &str) -> StorageResult<Self> {
        let pool = PgPoolOptions::new()
            .max_connections(5)
            .acquire_timeout(std::time::Duration::from_secs(5))
            .connect(url)
            .await?;
        let store = Self { pool };
        store.run_migrations().await?;
        Ok(store)
    }

    /// Runs pending migrations; safe to call repeatedly.
    pub async fn run_migrations(&self) -> StorageResult<()> {
        MIGRATIONS.run(&self.pool).await?;
        Ok(())
    }

    /// The underlying pool (tests and higher layers with raw SQL needs).
    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    // ---- tenants ----

    /// Inserts or updates a tenant row.
    pub async fn upsert_tenant(&self, tenant: &TenantRecord) -> StorageResult<()> {
        sqlx::query(
            "INSERT INTO tenants (id, name, created_at) VALUES ($1, $2, $3)
             ON CONFLICT (id) DO UPDATE SET name = EXCLUDED.name, created_at = EXCLUDED.created_at",
        )
        .bind(tenant.id.as_str())
        .bind(&tenant.name)
        .bind(tenant.created_at.as_millis())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Fetches one tenant, or [`StorageError::NotFound`].
    pub async fn get_tenant(&self, id: &TenantId) -> StorageResult<TenantRecord> {
        let row = sqlx::query("SELECT id, name, created_at FROM tenants WHERE id = $1")
            .bind(id.as_str())
            .fetch_optional(&self.pool)
            .await?
            .ok_or_else(|| StorageError::NotFound {
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
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Fetches one account, or [`StorageError::NotFound`].
    pub async fn get_account(&self, id: &AccountId) -> StorageResult<Account> {
        let row = sqlx::query(
            "SELECT id, tenant_id, platform, state, state_note, updated_at
             FROM accounts WHERE id = $1",
        )
        .bind(id.as_str())
        .fetch_optional(&self.pool)
        .await?
        .ok_or_else(|| StorageError::NotFound {
            what: "account",
            id: id.as_str().to_owned(),
        })?;
        map_account(&row)
    }

    /// Lists a tenant's accounts, ordered by id.
    pub async fn list_accounts(&self, tenant: &TenantId) -> StorageResult<Vec<Account>> {
        let rows = sqlx::query(
            "SELECT id, tenant_id, platform, state, state_note, updated_at
             FROM accounts WHERE tenant_id = $1 ORDER BY id",
        )
        .bind(tenant.as_str())
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(map_account).collect()
    }

    /// Updates an account's state/note and returns the fresh row.
    pub async fn update_account_state(
        &self,
        id: &AccountId,
        state: AccountState,
        state_note: Option<String>,
        at: UnixMillis,
    ) -> StorageResult<Account> {
        sqlx::query(
            "UPDATE accounts SET state = $2, state_note = $3, updated_at = $4 WHERE id = $1",
        )
        .bind(id.as_str())
        .bind(state.as_str())
        .bind(state_note)
        .bind(at.as_millis())
        .execute(&self.pool)
        .await?;
        self.get_account(id).await
    }

    // ---- conversations ----

    /// Inserts or updates a conversation row.
    pub async fn upsert_conversation(&self, conversation: &Conversation) -> StorageResult<()> {
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
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Fetches one conversation, or [`StorageError::NotFound`].
    pub async fn get_conversation(&self, id: &ConversationId) -> StorageResult<Conversation> {
        let row = sqlx::query(
            "SELECT id, account_id, tenant_id, updated_at
             FROM conversations WHERE id = $1",
        )
        .bind(id.as_str())
        .fetch_optional(&self.pool)
        .await?
        .ok_or_else(|| StorageError::NotFound {
            what: "conversation",
            id: id.as_str().to_owned(),
        })?;
        Ok(Conversation {
            id: ConversationId::parse(row.try_get::<String, _>("id")?)?,
            account_id: AccountId::parse(row.try_get::<String, _>("account_id")?)?,
            tenant_id: TenantId::parse(row.try_get::<String, _>("tenant_id")?)?,
            updated_at: UnixMillis::from_millis(row.try_get::<i64, _>("updated_at")?)?,
        })
    }

    /// Lists a tenant+account's conversations, most recently updated first.
    pub async fn list_conversations(
        &self,
        tenant: &TenantId,
        account: &AccountId,
    ) -> StorageResult<Vec<Conversation>> {
        let rows = sqlx::query(
            "SELECT id, account_id, tenant_id, updated_at
             FROM conversations
             WHERE tenant_id = $1 AND account_id = $2
             ORDER BY updated_at DESC, id",
        )
        .bind(tenant.as_str())
        .bind(account.as_str())
        .fetch_all(&self.pool)
        .await?;
        rows.iter()
            .map(|row| {
                Ok(Conversation {
                    id: ConversationId::parse(row.try_get::<String, _>("id")?)?,
                    account_id: AccountId::parse(row.try_get::<String, _>("account_id")?)?,
                    tenant_id: TenantId::parse(row.try_get::<String, _>("tenant_id")?)?,
                    updated_at: UnixMillis::from_millis(row.try_get::<i64, _>("updated_at")?)?,
                })
            })
            .collect()
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
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.is_some())
    }

    /// Lists one conversation's messages, oldest first.
    pub async fn list_messages(
        &self,
        tenant: &TenantId,
        conversation: &ConversationId,
    ) -> StorageResult<Vec<Message>> {
        let rows = sqlx::query(
            "SELECT id, conversation_id, tenant_id, direction, text, sent_at
             FROM messages
             WHERE tenant_id = $1 AND conversation_id = $2
             ORDER BY sent_at ASC, id",
        )
        .bind(tenant.as_str())
        .bind(conversation.as_str())
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(map_message).collect()
    }

    // ---- products ----

    /// Inserts or updates a product row.
    pub async fn upsert_product(&self, product: &Product) -> StorageResult<()> {
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
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Lists a tenant's products, ordered by id.
    pub async fn list_products(&self, tenant: &TenantId) -> StorageResult<Vec<Product>> {
        let rows = sqlx::query(
            "SELECT id, tenant_id, title, price_cents, currency, status, updated_at
             FROM products WHERE tenant_id = $1 ORDER BY id",
        )
        .bind(tenant.as_str())
        .fetch_all(&self.pool)
        .await?;
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
