//! Deterministic Mock services and API keys (the only CI target, §7.3).
//!
//! Everything lives in memory: two tenants with disjoint data so that
//! tenant isolation is observable, an append-only event log, and an
//! idempotency map for the send path. No upstream connection is ever
//! made; `send` simulates the Mock provider accepting the message.

use std::collections::BTreeMap;
use std::sync::Mutex;

use crate::auth::{ApiKeyRecord, ApiKeyStore, AuthContext, Scope};
use crate::services::{
    Account, AccountService, CatalogService, Conversation, EventEnvelope, EventService, Message,
    MessageDirection, MessagingService, Operation, OperationAttempt, OperationService, Page,
    Product, SendMessageOutcome, ServiceError, UNKNOWN_STATE_NOTE,
};
use async_trait::async_trait;

/// API key of tenant `t-default` (all scopes), for tests and local runs.
pub const KEY_DEFAULT_TENANT: &str = "fp_mock_key_default_tenant";
/// API key of tenant `t-other` (read-only), for isolation tests.
pub const KEY_OTHER_TENANT: &str = "fp_mock_key_other_tenant_readonly";

/// Default page size applied when a tool omits `limit`.
pub const DEFAULT_PAGE_SIZE: usize = 20;
/// Maximum accepted page size.
pub const MAX_PAGE_SIZE: usize = 100;

#[derive(Default)]
struct TenantState {
    accounts: Vec<Account>,
    conversations: Vec<Conversation>,
    messages: Vec<Message>,
    products: Vec<Product>,
    operations: Vec<Operation>,
    events: Vec<EventEnvelope>,
    next_event_seq: u64,
}

#[derive(Default)]
struct SharedState {
    /// Per-tenant seed data and live state.
    tenants: BTreeMap<String, TenantState>,
    /// (tenant, idempotency key) -> stored message.
    sends: BTreeMap<(String, String), Message>,
    /// Counter for generated message ids.
    next_message_seq: u64,
}

/// In-memory Mock implementation of every service trait.
#[derive(Default, Clone)]
pub struct MockServices {
    state: std::sync::Arc<Mutex<SharedState>>,
}

/// Clamps a requested page size into `1..=MAX_PAGE_SIZE` with the default.
fn clamp_limit(limit: usize) -> usize {
    if limit == 0 {
        DEFAULT_PAGE_SIZE
    } else {
        limit.min(MAX_PAGE_SIZE)
    }
}

/// Parses a mock cursor (plain offset string) and guards against garbage.
fn parse_cursor(cursor: &Option<String>) -> Result<usize, ServiceError> {
    match cursor {
        None => Ok(0),
        Some(raw) => raw
            .parse::<usize>()
            .map_err(|_| ServiceError::InvalidArgument {
                reason: format!("cursor is not a valid pagination cursor: {raw}"),
            }),
    }
}

/// Builds a mock cursor (plain offset string).
fn make_cursor(offset: usize) -> String {
    offset.to_string()
}

impl MockServices {
    /// Creates the service with deterministic seed data.
    pub fn seeded() -> Self {
        let mut state = SharedState {
            next_message_seq: 100,
            ..Default::default()
        };
        seed_default_tenant(&mut state);
        seed_other_tenant(&mut state);
        Self {
            state: std::sync::Arc::new(Mutex::new(state)),
        }
    }

    /// Returns the API key store matching the seeded data.
    pub fn bootstrap_keys() -> ApiKeyStore {
        let mut keys = ApiKeyStore::new();
        keys.insert(
            KEY_DEFAULT_TENANT.to_owned(),
            ApiKeyRecord {
                tenant_id: "t-default".to_owned(),
                scopes: Scope::ALL.to_vec(),
            },
        );
        keys.insert(
            KEY_OTHER_TENANT.to_owned(),
            ApiKeyRecord {
                tenant_id: "t-other".to_owned(),
                scopes: vec![
                    Scope::ACCOUNTS_READ,
                    Scope::CONVERSATIONS_READ,
                    Scope::CATALOG_READ,
                    Scope::OPERATIONS_READ,
                    Scope::EVENTS_READ,
                ],
            },
        );
        keys
    }

    fn with_tenant<T>(
        &self,
        ctx: &AuthContext,
        f: impl FnOnce(&mut TenantState) -> Result<T, ServiceError>,
    ) -> Result<T, ServiceError> {
        let mut guard = self.state.lock().expect("mock state mutex poisoned");
        let tenant = guard
            .tenants
            .get_mut(&ctx.tenant_id)
            .ok_or(ServiceError::NotFound {
                what: "tenant",
                id: ctx.tenant_id.clone(),
            })?;
        f(tenant)
    }
}

fn seed_default_tenant(state: &mut SharedState) {
    let t = state.tenants.entry("t-default".to_owned()).or_default();
    t.accounts = vec![
        Account {
            account_id: "acc-100".into(),
            tenant_id: "t-default".into(),
            platform: "mock".into(),
            state: "ONLINE".into(),
            state_note: None,
            updated_at: "2026-11-25T08:00:00Z".into(),
        },
        Account {
            account_id: "acc-101".into(),
            tenant_id: "t-default".into(),
            platform: "mock".into(),
            state: "DEGRADED".into(),
            state_note: Some("mock fault switch: intermittent disconnects".into()),
            updated_at: "2026-11-25T07:30:00Z".into(),
        },
        Account {
            account_id: "acc-102".into(),
            tenant_id: "t-default".into(),
            platform: "mock".into(),
            state: "UNKNOWN".into(),
            state_note: Some(UNKNOWN_STATE_NOTE.into()),
            updated_at: "2026-11-25T06:45:00Z".into(),
        },
    ];
    t.conversations = vec![
        Conversation {
            conversation_id: "conv-1".into(),
            account_id: "acc-100".into(),
            peer_id: "buyer-7".into(),
            summary: "buyer asks whether the item ships today".into(),
            updated_at: "2026-11-25T08:10:00Z".into(),
        },
        Conversation {
            conversation_id: "conv-2".into(),
            account_id: "acc-101".into(),
            peer_id: "buyer-12".into(),
            summary: "price negotiation on listing p-2".into(),
            updated_at: "2026-11-25T07:35:00Z".into(),
        },
    ];
    t.messages = vec![
        Message {
            message_id: "m-1".into(),
            conversation_id: "conv-1".into(),
            direction: MessageDirection::Inbound,
            text: "你好，这个今天能发货吗？".into(),
            sent_at: "2026-11-25T08:05:00Z".into(),
        },
        Message {
            message_id: "m-2".into(),
            conversation_id: "conv-1".into(),
            direction: MessageDirection::Outbound,
            text: "可以，下午发出。".into(),
            sent_at: "2026-11-25T08:10:00Z".into(),
        },
        Message {
            message_id: "m-3".into(),
            conversation_id: "conv-2".into(),
            direction: MessageDirection::Inbound,
            text: "能便宜 20 吗？".into(),
            sent_at: "2026-11-25T07:35:00Z".into(),
        },
    ];
    t.products = vec![
        Product {
            product_id: "p-1".into(),
            title: "Mechanical keyboard (Mock)".into(),
            price_cents: 29_900,
            currency: "CNY".into(),
            status: "listed".into(),
            updated_at: "2026-11-24T10:00:00Z".into(),
        },
        Product {
            product_id: "p-2".into(),
            title: "USB-C dock (Mock)".into(),
            price_cents: 45_000,
            currency: "CNY".into(),
            status: "listed".into(),
            updated_at: "2026-11-24T10:05:00Z".into(),
        },
        Product {
            product_id: "p-3".into(),
            title: "Desk lamp (Mock)".into(),
            price_cents: 12_900,
            currency: "CNY".into(),
            status: "paused".into(),
            updated_at: "2026-11-23T18:00:00Z".into(),
        },
    ];
    t.operations = vec![
        Operation {
            operation_id: "op-1".into(),
            kind: "send_message".into(),
            account_id: "acc-100".into(),
            status: "succeeded".into(),
            created_at: "2026-11-25T08:10:00Z".into(),
            attempts: vec![OperationAttempt {
                attempt_id: "a-1".into(),
                started_at: "2026-11-25T08:10:00Z".into(),
                outcome: "succeeded".into(),
            }],
        },
        Operation {
            operation_id: "op-2".into(),
            kind: "relist_product".into(),
            account_id: "acc-101".into(),
            status: "in_progress".into(),
            created_at: "2026-11-25T07:40:00Z".into(),
            attempts: vec![OperationAttempt {
                attempt_id: "a-2".into(),
                started_at: "2026-11-25T07:40:00Z".into(),
                outcome: "in_flight".into(),
            }],
        },
    ];
    t.next_event_seq = 5;
    t.events = vec![
        EventEnvelope {
            event_id: "evt-1".into(),
            seq: 1,
            event_type: "account.state_changed".into(),
            account_id: "acc-100".into(),
            occurred_at: "2026-11-25T08:00:00Z".into(),
            payload: serde_json::json!({"from": "RECOVERING", "to": "ONLINE"}),
        },
        EventEnvelope {
            event_id: "evt-2".into(),
            seq: 2,
            event_type: "message.received".into(),
            account_id: "acc-100".into(),
            occurred_at: "2026-11-25T08:05:00Z".into(),
            payload: serde_json::json!({"conversationId": "conv-1", "messageId": "m-1"}),
        },
        EventEnvelope {
            event_id: "evt-3".into(),
            seq: 3,
            event_type: "message.sent".into(),
            account_id: "acc-100".into(),
            occurred_at: "2026-11-25T08:10:00Z".into(),
            payload: serde_json::json!({"conversationId": "conv-1", "messageId": "m-2"}),
        },
        EventEnvelope {
            event_id: "evt-4".into(),
            seq: 4,
            event_type: "account.state_changed".into(),
            account_id: "acc-101".into(),
            occurred_at: "2026-11-25T07:30:00Z".into(),
            payload: serde_json::json!({"from": "ONLINE", "to": "DEGRADED"}),
        },
        EventEnvelope {
            event_id: "evt-5".into(),
            seq: 5,
            event_type: "operation.created".into(),
            account_id: "acc-101".into(),
            occurred_at: "2026-11-25T07:40:00Z".into(),
            payload: serde_json::json!({"operationId": "op-2", "kind": "relist_product"}),
        },
    ];
}

fn seed_other_tenant(state: &mut SharedState) {
    let t = state.tenants.entry("t-other".to_owned()).or_default();
    t.accounts = vec![Account {
        account_id: "acc-900".into(),
        tenant_id: "t-other".into(),
        platform: "mock".into(),
        state: "ONLINE".into(),
        state_note: None,
        updated_at: "2026-11-25T09:00:00Z".into(),
    }];
    t.conversations = vec![Conversation {
        conversation_id: "conv-9".into(),
        account_id: "acc-900".into(),
        peer_id: "buyer-90".into(),
        summary: "separate tenant fixture".into(),
        updated_at: "2026-11-25T09:05:00Z".into(),
    }];
    t.products = vec![Product {
        product_id: "p-90".into(),
        title: "Other-tenant fixture (Mock)".into(),
        price_cents: 9_900,
        currency: "CNY".into(),
        status: "listed".into(),
        updated_at: "2026-11-24T09:00:00Z".into(),
    }];
    t.operations = vec![Operation {
        operation_id: "op-9".into(),
        kind: "send_message".into(),
        account_id: "acc-900".into(),
        status: "succeeded".into(),
        created_at: "2026-11-25T09:05:00Z".into(),
        attempts: vec![OperationAttempt {
            attempt_id: "a-9".into(),
            started_at: "2026-11-25T09:05:00Z".into(),
            outcome: "succeeded".into(),
        }],
    }];
    t.next_event_seq = 1;
    t.events = vec![EventEnvelope {
        event_id: "evt-901".into(),
        seq: 1,
        event_type: "account.state_changed".into(),
        account_id: "acc-900".into(),
        occurred_at: "2026-11-25T09:00:00Z".into(),
        payload: serde_json::json!({"from": "AUTHORIZING", "to": "ONLINE"}),
    }];
}

#[async_trait]
impl AccountService for MockServices {
    async fn list(
        &self,
        ctx: &AuthContext,
        cursor: Option<String>,
        limit: usize,
    ) -> Result<Page<Account>, ServiceError> {
        self.with_tenant(ctx, |t| {
            let start = parse_cursor(&cursor)?;
            let limit = clamp_limit(limit);
            let end = (start + limit).min(t.accounts.len());
            let next = if end < t.accounts.len() {
                Some(make_cursor(end))
            } else {
                None
            };
            Ok(Page {
                items: t.accounts[start..end].to_vec(),
                next_cursor: next,
            })
        })
    }

    async fn get(&self, ctx: &AuthContext, account_id: &str) -> Result<Account, ServiceError> {
        self.with_tenant(ctx, |t| {
            t.accounts
                .iter()
                .find(|a| a.account_id == account_id)
                .cloned()
                .ok_or(ServiceError::NotFound {
                    what: "account",
                    id: account_id.to_owned(),
                })
        })
    }
}

#[async_trait]
impl MessagingService for MockServices {
    async fn list_conversations(
        &self,
        ctx: &AuthContext,
        account_id: Option<&str>,
        cursor: Option<String>,
        limit: usize,
    ) -> Result<Page<Conversation>, ServiceError> {
        self.with_tenant(ctx, |t| {
            if let Some(id) = account_id
                && !t.accounts.iter().any(|a| a.account_id == id)
            {
                return Err(ServiceError::NotFound {
                    what: "account",
                    id: id.to_owned(),
                });
            }
            let filtered: Vec<Conversation> = t
                .conversations
                .iter()
                .filter(|c| account_id.is_none_or(|id| c.account_id == id))
                .cloned()
                .collect();
            let start = parse_cursor(&cursor)?;
            let limit = clamp_limit(limit);
            let end = (start + limit).min(filtered.len());
            let next = if end < filtered.len() {
                Some(make_cursor(end))
            } else {
                None
            };
            Ok(Page {
                items: filtered[start..end].to_vec(),
                next_cursor: next,
            })
        })
    }

    async fn list_messages(
        &self,
        ctx: &AuthContext,
        conversation_id: &str,
        cursor: Option<String>,
        limit: usize,
    ) -> Result<Page<Message>, ServiceError> {
        self.with_tenant(ctx, |t| {
            if !t
                .conversations
                .iter()
                .any(|c| c.conversation_id == conversation_id)
            {
                return Err(ServiceError::NotFound {
                    what: "conversation",
                    id: conversation_id.to_owned(),
                });
            }
            let start = parse_cursor(&cursor)?;
            let limit = clamp_limit(limit);
            // Only messages of the requested conversation, oldest first.
            let scoped: Vec<Message> = t
                .messages
                .iter()
                .filter(|m| m.conversation_id == conversation_id)
                .cloned()
                .collect();
            let end = (start + limit).min(scoped.len());
            let next = if end < scoped.len() {
                Some(make_cursor(end))
            } else {
                None
            };
            Ok(Page {
                items: scoped[start..end].to_vec(),
                next_cursor: next,
            })
        })
    }

    async fn send(
        &self,
        ctx: &AuthContext,
        conversation_id: &str,
        text: &str,
        idempotency_key: &str,
    ) -> Result<SendMessageOutcome, ServiceError> {
        if text.trim().is_empty() {
            return Err(ServiceError::InvalidArgument {
                reason: "text must not be empty".into(),
            });
        }
        if idempotency_key.trim().is_empty() {
            return Err(ServiceError::InvalidArgument {
                reason: "idempotency_key must not be empty".into(),
            });
        }
        let mut guard = self.state.lock().expect("mock state mutex poisoned");
        let dedup_key = (ctx.tenant_id.clone(), idempotency_key.to_owned());
        if let Some(original) = guard.sends.get(&dedup_key) {
            return Ok(SendMessageOutcome {
                message: original.clone(),
                deduplicated: true,
            });
        }
        // Read the sequence before taking the mutable tenant borrow below.
        let next_message_seq = guard.next_message_seq;
        let message = {
            let tenant = guard
                .tenants
                .get_mut(&ctx.tenant_id)
                .ok_or(ServiceError::NotFound {
                    what: "tenant",
                    id: ctx.tenant_id.clone(),
                })?;
            let conversation = tenant
                .conversations
                .iter()
                .find(|c| c.conversation_id == conversation_id)
                .ok_or(ServiceError::NotFound {
                    what: "conversation",
                    id: conversation_id.to_owned(),
                })?;
            let account_id = conversation.account_id.clone();
            let message = Message {
                message_id: format!("m-{next_message_seq}"),
                conversation_id: conversation_id.to_owned(),
                direction: MessageDirection::Outbound,
                text: text.to_owned(),
                sent_at: "2026-11-25T12:00:00Z".into(),
            };
            tenant.messages.push(message.clone());
            tenant.next_event_seq += 1;
            tenant.events.push(EventEnvelope {
                event_id: format!("evt-{}", tenant.next_event_seq),
                seq: tenant.next_event_seq,
                event_type: "message.sent".into(),
                account_id,
                occurred_at: "2026-11-25T12:00:00Z".into(),
                payload: serde_json::json!({
                    "conversationId": conversation_id,
                    "messageId": message.message_id,
                }),
            });
            message
        };
        guard.next_message_seq = next_message_seq + 1;
        guard.sends.insert(dedup_key, message.clone());
        Ok(SendMessageOutcome {
            message,
            deduplicated: false,
        })
    }
}

#[async_trait]
impl CatalogService for MockServices {
    async fn list(
        &self,
        ctx: &AuthContext,
        cursor: Option<String>,
        limit: usize,
    ) -> Result<Page<Product>, ServiceError> {
        self.with_tenant(ctx, |t| {
            let start = parse_cursor(&cursor)?;
            let limit = clamp_limit(limit);
            let end = (start + limit).min(t.products.len());
            let next = if end < t.products.len() {
                Some(make_cursor(end))
            } else {
                None
            };
            Ok(Page {
                items: t.products[start..end].to_vec(),
                next_cursor: next,
            })
        })
    }

    async fn get(&self, ctx: &AuthContext, product_id: &str) -> Result<Product, ServiceError> {
        self.with_tenant(ctx, |t| {
            t.products
                .iter()
                .find(|p| p.product_id == product_id)
                .cloned()
                .ok_or(ServiceError::NotFound {
                    what: "product",
                    id: product_id.to_owned(),
                })
        })
    }
}

#[async_trait]
impl OperationService for MockServices {
    async fn list(
        &self,
        ctx: &AuthContext,
        cursor: Option<String>,
        limit: usize,
    ) -> Result<Page<Operation>, ServiceError> {
        self.with_tenant(ctx, |t| {
            let start = parse_cursor(&cursor)?;
            let limit = clamp_limit(limit);
            let end = (start + limit).min(t.operations.len());
            let next = if end < t.operations.len() {
                Some(make_cursor(end))
            } else {
                None
            };
            Ok(Page {
                items: t.operations[start..end].to_vec(),
                next_cursor: next,
            })
        })
    }

    async fn get(&self, ctx: &AuthContext, operation_id: &str) -> Result<Operation, ServiceError> {
        self.with_tenant(ctx, |t| {
            t.operations
                .iter()
                .find(|o| o.operation_id == operation_id)
                .cloned()
                .ok_or(ServiceError::NotFound {
                    what: "operation",
                    id: operation_id.to_owned(),
                })
        })
    }
}

#[async_trait]
impl EventService for MockServices {
    async fn recent(
        &self,
        ctx: &AuthContext,
        limit: usize,
    ) -> Result<Vec<EventEnvelope>, ServiceError> {
        self.with_tenant(ctx, |t| {
            let limit = clamp_limit(limit);
            let start = t.events.len().saturating_sub(limit);
            Ok(t.events[start..].to_vec())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx(tenant: &str) -> AuthContext {
        AuthContext {
            tenant_id: tenant.to_owned(),
            scopes: Scope::ALL.to_vec(),
            transport: crate::auth::Transport::Stdio,
        }
    }

    #[tokio::test]
    async fn tenant_sees_only_own_accounts() {
        let services = MockServices::seeded();
        let default = AccountService::list(&services, &ctx("t-default"), None, 20)
            .await
            .unwrap();
        let other = AccountService::list(&services, &ctx("t-other"), None, 20)
            .await
            .unwrap();
        assert_eq!(default.items.len(), 3);
        assert_eq!(other.items.len(), 1);
        assert!(default.items.iter().all(|a| a.tenant_id == "t-default"));
    }

    #[tokio::test]
    async fn cross_tenant_get_is_not_found() {
        let services = MockServices::seeded();
        let err = AccountService::get(&services, &ctx("t-other"), "acc-100")
            .await
            .unwrap_err();
        assert_eq!(
            err,
            ServiceError::NotFound {
                what: "account",
                id: "acc-100".into()
            }
        );
    }

    #[tokio::test]
    async fn send_is_idempotent_per_key() {
        let services = MockServices::seeded();
        let first = services
            .send(&ctx("t-default"), "conv-1", "ship it", "idem-1")
            .await
            .unwrap();
        assert!(!first.deduplicated);
        let second = services
            .send(&ctx("t-default"), "conv-1", "ship it", "idem-1")
            .await
            .unwrap();
        assert!(second.deduplicated);
        assert_eq!(first.message.message_id, second.message.message_id);
        // exactly one message appended
        let page = services
            .list_messages(&ctx("t-default"), "conv-1", None, 100)
            .await
            .unwrap();
        assert_eq!(page.items.iter().filter(|m| m.text == "ship it").count(), 1);
        // different key -> new message
        let third = services
            .send(&ctx("t-default"), "conv-1", "ship it again", "idem-2")
            .await
            .unwrap();
        assert!(!third.deduplicated);
        assert_ne!(third.message.message_id, first.message.message_id);
    }

    #[tokio::test]
    async fn list_messages_is_scoped_to_one_conversation() {
        let services = MockServices::seeded();
        let page = services
            .list_messages(&ctx("t-default"), "conv-1", None, 100)
            .await
            .unwrap();
        assert_eq!(page.items.len(), 2);
        assert!(
            page.items.iter().all(|m| m.conversation_id == "conv-1"),
            "messages of other conversations must not leak in"
        );
        assert!(page.next_cursor.is_none());
    }

    #[tokio::test]
    async fn unknown_account_state_carries_note() {
        let services = MockServices::seeded();
        let account = AccountService::get(&services, &ctx("t-default"), "acc-102")
            .await
            .unwrap();
        assert_eq!(account.state, "UNKNOWN");
        assert!(account.state_note.unwrap().contains("UNKNOWN"));
    }

    #[tokio::test]
    async fn pagination_clamps_and_cursors() {
        let services = MockServices::seeded();
        let page = AccountService::list(&services, &ctx("t-default"), None, 2)
            .await
            .unwrap();
        assert_eq!(page.items.len(), 2);
        assert_eq!(page.next_cursor.as_deref(), Some("2"));
        let next = AccountService::list(&services, &ctx("t-default"), page.next_cursor.clone(), 2)
            .await
            .unwrap();
        assert_eq!(next.items.len(), 1);
        assert_eq!(next.next_cursor, None);
        let garbage =
            AccountService::list(&services, &ctx("t-default"), Some("nope".into()), 5).await;
        assert!(matches!(garbage, Err(ServiceError::InvalidArgument { .. })));
    }
}
