//! The MCP tool face: 1:1 wrappers over the REST service semantics.
//!
//! Every tool performs the same three steps: authenticate the presented
//! API key ([`crate::auth`]), check the required scope, and delegate to
//! the [`crate::services`] seam. Tools carry no business logic and never
//! touch a provider directly; write tools go through the Mock provider
//! path only (§7.3, F032 stays disabled).

use std::borrow::Cow;
use std::sync::Arc;

use rmcp::{
    ErrorData as McpError, RoleServer, ServerHandler,
    handler::server::wrapper::Parameters,
    model::{
        CallToolResult, ContentBlock, ErrorCode, Implementation, ProtocolVersion,
        ServerCapabilities, ServerConfig,
    },
    schemars,
    service::RequestContext,
    tool, tool_handler, tool_router,
};

use crate::auth::{self, ApiKeyStore, AuthContext, Scope, Transport};
use crate::mock::MockServices;
use crate::redact;
use crate::services::{
    AccountService, CatalogService, EventService, MessagingService, OperationService, ServiceError,
};

/// All service traits the MCP face needs, bundled for construction.
#[derive(Clone)]
pub struct ServiceBundle {
    /// Accounts read path.
    pub accounts: Arc<dyn AccountService>,
    /// Conversations, messages and the Mock send path.
    pub messaging: Arc<dyn MessagingService>,
    /// Product catalog read path.
    pub catalog: Arc<dyn CatalogService>,
    /// Operations and attempts read path.
    pub operations: Arc<dyn OperationService>,
    /// Event stream read path.
    pub events: Arc<dyn EventService>,
}

impl ServiceBundle {
    /// Bundles Mock implementations of every service (the M0 default).
    pub fn mock() -> Self {
        let mock: Arc<MockServices> = Arc::new(MockServices::seeded());
        Self {
            accounts: mock.clone(),
            messaging: mock.clone(),
            catalog: mock.clone(),
            operations: mock.clone(),
            events: mock,
        }
    }
}

/// The MCP server handler exposing the Fishpond tool face.
///
/// The tool router is built per invocation by the `#[tool_handler]` macro
/// (it is stateless), so the handler only carries its services and keys.
#[derive(Clone)]
pub struct FishpondMcp {
    services: ServiceBundle,
    keys: Arc<ApiKeyStore>,
    /// Key presented via `FISHPOND_API_KEY` for the stdio transport.
    stdio_key: Option<String>,
}

#[tool_router]
impl FishpondMcp {
    /// Creates a handler over `services` with the given key store.
    ///
    /// `stdio_key` is the key read from `FISHPOND_API_KEY` by the CLI; it
    /// is only consulted when the session runs over stdio (no request
    /// headers are available there).
    pub fn new(services: ServiceBundle, keys: ApiKeyStore, stdio_key: Option<String>) -> Self {
        Self {
            services,
            keys: Arc::new(keys),
            stdio_key,
        }
    }

    /// Creates a handler over the seeded Mock services and Mock key store.
    pub fn mock(stdio_key: Option<String>) -> Self {
        Self::new(
            ServiceBundle::mock(),
            MockServices::bootstrap_keys(),
            stdio_key,
        )
    }

    fn authenticate(&self, ctx: &RequestContext<RoleServer>) -> Result<AuthContext, McpError> {
        let (key, transport) = match ctx.extensions.get::<axum::http::request::Parts>() {
            Some(parts) => (
                auth::extract_key_from_headers(&parts.headers),
                Transport::Http,
            ),
            None => (self.stdio_key.clone(), Transport::Stdio),
        };
        let Some(key) = key else {
            tracing::warn!(target: "fishpond::mcp", transport = ?transport, "authentication failed: no API key presented");
            return Err(unauthenticated());
        };
        match self.keys.resolve(&key) {
            Some(record) => {
                tracing::debug!(
                    target: "fishpond::mcp",
                    key = %redact::key_display(&key),
                    tenant = %record.tenant_id,
                    transport = ?transport,
                    "api key accepted"
                );
                Ok(AuthContext {
                    tenant_id: record.tenant_id.clone(),
                    scopes: record.scopes.clone(),
                    transport,
                })
            }
            None => {
                tracing::warn!(
                    target: "fishpond::mcp",
                    key = %redact::key_display(&key),
                    transport = ?transport,
                    "authentication failed: unknown API key"
                );
                Err(unauthenticated())
            }
        }
    }

    // ---- accounts ------------------------------------------------------

    #[tool(description = "List accounts of the authenticated tenant. \
Idempotent read; results are scoped to the tenant of the API key, so other tenants' accounts are never visible. \
Account `state` follows the Fishpond state machine; `UNKNOWN` means the observed state could not be verified.")]
    async fn list_accounts(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(args): Parameters<ListArgs>,
    ) -> Result<CallToolResult, McpError> {
        let auth = self.authenticate(&ctx)?;
        require_scope(&auth, Scope::ACCOUNTS_READ)?;
        let limit = args.limit();
        let cursor = args.cursor;
        let page = self
            .services
            .accounts
            .list(&auth, cursor, limit)
            .await
            .map_err(service_error)?;
        ok_json(&page)
    }

    #[tool(
        description = "Get one account by id, including its state-machine string. \
Idempotent read, tenant-scoped: accounts of other tenants are reported as not found. \
`UNKNOWN` state means the observed state could not be verified (neither confirmed online nor offline)."
    )]
    async fn get_account(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(args): Parameters<GetByIdArgs>,
    ) -> Result<CallToolResult, McpError> {
        let auth = self.authenticate(&ctx)?;
        require_scope(&auth, Scope::ACCOUNTS_READ)?;
        let account = self
            .services
            .accounts
            .get(&auth, &args.id)
            .await
            .map_err(service_error)?;
        ok_json(&account)
    }

    // ---- conversations & messages --------------------------------------

    #[tool(
        description = "List conversations, optionally filtered by account id. \
Idempotent read, tenant-scoped by the API key."
    )]
    async fn list_conversations(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(args): Parameters<ListConversationsArgs>,
    ) -> Result<CallToolResult, McpError> {
        let auth = self.authenticate(&ctx)?;
        require_scope(&auth, Scope::CONVERSATIONS_READ)?;
        let limit = args.limit();
        let account_id = args.account_id.clone();
        let cursor = args.cursor;
        let page = self
            .services
            .messaging
            .list_conversations(&auth, account_id.as_deref(), cursor, limit)
            .await
            .map_err(service_error)?;
        ok_json(&page)
    }

    #[tool(description = "List messages of one conversation, oldest first. \
Idempotent read, tenant-scoped by the API key.")]
    async fn list_messages(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(args): Parameters<ListMessagesArgs>,
    ) -> Result<CallToolResult, McpError> {
        let auth = self.authenticate(&ctx)?;
        require_scope(&auth, Scope::CONVERSATIONS_READ)?;
        let limit = args.limit();
        let conversation_id = args.conversation_id.clone();
        let cursor = args.cursor;
        let page = self
            .services
            .messaging
            .list_messages(&auth, &conversation_id, cursor, limit)
            .await
            .map_err(service_error)?;
        ok_json(&page)
    }

    #[tool(
        description = "Send a message in a conversation through the provider. \
M0 routes through the Mock provider only; real providers stay disabled. \
Idempotency: `idempotency_key` is REQUIRED and acts as the REST Idempotency-Key header; \
replaying the same key returns the original message with `deduplicated: true` and never double-sends. \
Tenant-scoped by the API key; requires the conversations:write scope."
    )]
    async fn send_message(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(args): Parameters<SendMessageArgs>,
    ) -> Result<CallToolResult, McpError> {
        let auth = self.authenticate(&ctx)?;
        require_scope(&auth, Scope::CONVERSATIONS_WRITE)?;
        let outcome = self
            .services
            .messaging
            .send(
                &auth,
                &args.conversation_id,
                &args.text,
                &args.idempotency_key,
            )
            .await
            .map_err(service_error)?;
        ok_json(&outcome)
    }

    // ---- catalog --------------------------------------------------------

    #[tool(description = "List products of the authenticated tenant. \
Idempotent read, tenant-scoped by the API key.")]
    async fn list_products(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(args): Parameters<ListArgs>,
    ) -> Result<CallToolResult, McpError> {
        let auth = self.authenticate(&ctx)?;
        require_scope(&auth, Scope::CATALOG_READ)?;
        let limit = args.limit();
        let cursor = args.cursor;
        let page = self
            .services
            .catalog
            .list(&auth, cursor, limit)
            .await
            .map_err(service_error)?;
        ok_json(&page)
    }

    #[tool(description = "Get one product by id. \
Idempotent read, tenant-scoped: products of other tenants are reported as not found.")]
    async fn get_product(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(args): Parameters<GetByIdArgs>,
    ) -> Result<CallToolResult, McpError> {
        let auth = self.authenticate(&ctx)?;
        require_scope(&auth, Scope::CATALOG_READ)?;
        let product = self
            .services
            .catalog
            .get(&auth, &args.id)
            .await
            .map_err(service_error)?;
        ok_json(&product)
    }

    // ---- operations ------------------------------------------------------

    #[tool(
        description = "List operations (intents) of the authenticated tenant. \
Idempotent read, tenant-scoped by the API key."
    )]
    async fn list_operations(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(args): Parameters<ListArgs>,
    ) -> Result<CallToolResult, McpError> {
        let auth = self.authenticate(&ctx)?;
        require_scope(&auth, Scope::OPERATIONS_READ)?;
        let limit = args.limit();
        let cursor = args.cursor;
        let page = self
            .services
            .operations
            .list(&auth, cursor, limit)
            .await
            .map_err(service_error)?;
        ok_json(&page)
    }

    #[tool(
        description = "Get one operation by id, including its attempt summaries. \
Idempotent read, tenant-scoped: operations of other tenants are reported as not found."
    )]
    async fn get_operation(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(args): Parameters<GetByIdArgs>,
    ) -> Result<CallToolResult, McpError> {
        let auth = self.authenticate(&ctx)?;
        require_scope(&auth, Scope::OPERATIONS_READ)?;
        let operation = self
            .services
            .operations
            .get(&auth, &args.id)
            .await
            .map_err(service_error)?;
        ok_json(&operation)
    }

    // ---- events ----------------------------------------------------------

    #[tool(
        description = "Read recent events of the authenticated tenant (snapshot). \
Idempotent read, tenant-scoped by the API key. \
M0 returns a snapshot; live push as MCP notifications is planned for M1 once the event pipeline (F017) lands."
    )]
    async fn subscribe_events(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(args): Parameters<SubscribeEventsArgs>,
    ) -> Result<CallToolResult, McpError> {
        let auth = self.authenticate(&ctx)?;
        require_scope(&auth, Scope::EVENTS_READ)?;
        let events = self
            .services
            .events
            .recent(&auth, args.limit())
            .await
            .map_err(service_error)?;
        ok_json(&events)
    }
}

#[tool_handler]
impl ServerHandler for FishpondMcp {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::from_build_env())
            .with_protocol_version(ProtocolVersion::V_2024_11_05)
            .with_instructions(
                "Fishpond MCP face (Mock provider in M0). Tools wrap the REST /api/v1 semantics 1:1. \
Authorization: stdio sessions use the FISHPOND_API_KEY environment variable, HTTP sessions send \
the API key in the x-fishpond-api-key or Authorization: Bearer header. All reads are idempotent and \
tenant-scoped; send_message requires conversations:write and a caller-supplied idempotency_key. \
Account states follow the Fishpond state machine; UNKNOWN means the observed state could not be verified."
                    .to_owned(),
            )
    }
}

// ---- argument schemas ----------------------------------------------------

/// Shared pagination arguments.
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ListArgs {
    /// Opaque pagination cursor from a previous response.
    pub cursor: Option<String>,
    /// Maximum number of items to return (1-100, default 20).
    pub limit: Option<usize>,
}

impl ListArgs {
    fn limit(&self) -> usize {
        self.limit.unwrap_or(0)
    }
}

/// Lookup by resource id.
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct GetByIdArgs {
    /// Identifier of the resource to fetch.
    pub id: String,
}

/// Conversation listing with optional account filter.
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ListConversationsArgs {
    /// Only list conversations of this account.
    pub account_id: Option<String>,
    /// Opaque pagination cursor from a previous response.
    pub cursor: Option<String>,
    /// Maximum number of items to return (1-100, default 20).
    pub limit: Option<usize>,
}

impl ListConversationsArgs {
    fn limit(&self) -> usize {
        self.limit.unwrap_or(0)
    }
}

/// Message listing within one conversation.
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ListMessagesArgs {
    /// Conversation whose messages to list.
    pub conversation_id: String,
    /// Opaque pagination cursor from a previous response.
    pub cursor: Option<String>,
    /// Maximum number of items to return (1-100, default 20).
    pub limit: Option<usize>,
}

impl ListMessagesArgs {
    fn limit(&self) -> usize {
        self.limit.unwrap_or(0)
    }
}

/// Send-message arguments.
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SendMessageArgs {
    /// Target conversation id.
    pub conversation_id: String,
    /// Message body to send.
    pub text: String,
    /// REQUIRED idempotency key (REST Idempotency-Key header equivalent). \
    /// Replaying the same key returns the original message.
    pub idempotency_key: String,
}

/// Event snapshot arguments.
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SubscribeEventsArgs {
    /// Maximum number of events to return (1-100, default 20).
    pub limit: Option<usize>,
}

impl SubscribeEventsArgs {
    fn limit(&self) -> usize {
        self.limit.unwrap_or(0)
    }
}

// ---- helpers ---------------------------------------------------------------

fn unauthenticated() -> McpError {
    McpError::new(
        ErrorCode::INVALID_REQUEST,
        Cow::from(
            "unauthenticated: present a valid API key (FISHPOND_API_KEY for stdio, x-fishpond-api-key header for HTTP)",
        ),
        None,
    )
}

fn require_scope(auth: &AuthContext, scope: Scope) -> Result<(), McpError> {
    if auth.has(scope) {
        Ok(())
    } else {
        Err(McpError::new(
            ErrorCode::INVALID_REQUEST,
            Cow::from(format!(
                "permission denied: this tool requires the {} scope",
                scope
            )),
            None,
        ))
    }
}

fn service_error(err: ServiceError) -> McpError {
    // Messages are built from static text and resource ids only; they never
    // contain key material (pinned by the redaction tests).
    let (code, message) = match err {
        ServiceError::Unauthenticated => (
            ErrorCode::INVALID_REQUEST,
            "unauthenticated: present a valid API key".to_owned(),
        ),
        ServiceError::PermissionDenied { scope } => (
            ErrorCode::INVALID_REQUEST,
            format!("permission denied: missing scope {scope}"),
        ),
        ServiceError::NotFound { what, id } => (
            ErrorCode::RESOURCE_NOT_FOUND,
            format!("{what} not found: {id}"),
        ),
        ServiceError::InvalidArgument { reason } => (
            ErrorCode::INVALID_PARAMS,
            format!("invalid argument: {reason}"),
        ),
    };
    McpError::new(code, Cow::from(message), None)
}

fn ok_json<T: serde::Serialize + ?Sized>(value: &T) -> Result<CallToolResult, McpError> {
    let text = serde_json::to_string_pretty(value).map_err(|e| {
        McpError::new(
            ErrorCode::INTERNAL_ERROR,
            Cow::from(format!("failed to serialize tool output: {e}")),
            None,
        )
    })?;
    Ok(CallToolResult::success(vec![ContentBlock::text(text)]))
}
