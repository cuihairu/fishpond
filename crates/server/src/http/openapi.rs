//! The served OpenAPI 3.1 document (F025): the machine-readable
//! contract for this REST face. It is a static document built here —
//! no codegen dependency — and contract-tested byte-for-byte, so any
//! route change that forgets the document fails the build.

use axum::Json;
use serde_json::json;

use super::problem::PROBLEM_BASE;

/// Builds the OpenAPI document.
pub fn document() -> serde_json::Value {
    let problem_ref = json!({ "$ref": "#/components/schemas/Problem" });
    let path_param = json!({
        "name": "id", "in": "path", "required": true,
        "schema": { "type": "string" },
        "description": "Resource id"
    });
    let not_found = json!({
        "description": "Absent or foreign resource", "content": {
            "application/problem+json": { "schema": problem_ref }
        }
    });
    let conflict = json!({
        "description": "Illegal state transition or duplicate", "content": {
            "application/problem+json": { "schema": problem_ref }
        }
    });
    let invalid = json!({
        "description": "Malformed arguments", "content": {
            "application/problem+json": { "schema": problem_ref }
        }
    });
    let unauthorized = json!({
        "description": "Missing or unknown key", "content": {
            "application/problem+json": { "schema": problem_ref }
        }
    });
    let scope_denied = json!({
        "description": "Key lacks the required scope", "content": {
            "application/problem+json": { "schema": problem_ref }
        }
    });
    json!({
        "openapi": "3.1.0",
        "info": {
            "title": "fishpond REST",
            "version": env!("CARGO_PKG_VERSION"),
            "description": "Control-plane REST face: accounts, authorization sessions, \
                            operations, events. Errors are RFC 9457 problem+json documents."
        },
        "servers": [{ "url": "/" }],
        "paths": {
            "/healthz": {
                "get": {
                    "operationId": "health",
                    "summary": "Liveness probe",
                    "responses": {
                        "200": { "description": "The process is up", "content": {
                            "application/json": { "schema": { "$ref": "#/components/schemas/Health" } }
                        } }
                    }
                }
            },
            "/accounts": {
                "get": {
                    "operationId": "listAccounts",
                    "summary": "List the tenant's accounts (ordered by id)",
                    "security": [{ "bearerAuth": [] }, { "scope": "accounts:read" }],
                    "responses": {
                        "200": { "description": "The account list", "content": {
                            "application/json": { "schema": { "$ref": "#/components/schemas/AccountList" } }
                        } },
                        "401": unauthorized,
                        "403": scope_denied
                    }
                },
                "post": {
                    "operationId": "registerAccount",
                    "summary": "Register an account (OFFLINE)",
                    "security": [{ "bearerAuth": [] }],
                    "requestBody": { "required": true, "content": {
                        "application/json": { "schema": { "$ref": "#/components/schemas/RegisterAccount" } }
                    } },
                    "responses": {
                        "201": { "description": "The created account", "content": {
                            "application/json": { "schema": { "$ref": "#/components/schemas/Account" } }
                        } },
                        "401": unauthorized,
                        "403": scope_denied,
                        "409": conflict,
                        "422": invalid
                    }
                }
            },
            "/accounts/{id}": {
                "get": {
                    "operationId": "getAccount",
                    "summary": "One account",
                    "security": [{ "bearerAuth": [] }],
                    "parameters": [path_param],
                    "responses": {
                        "200": { "description": "The account", "content": {
                            "application/json": { "schema": { "$ref": "#/components/schemas/Account" } }
                        } },
                        "404": not_found
                    }
                }
            },
            "/accounts/{id}/state": {
                "post": {
                    "operationId": "setAccountState",
                    "summary": "One account state-machine step",
                    "security": [{ "bearerAuth": [] }],
                    "parameters": [path_param],
                    "requestBody": { "required": true, "content": {
                        "application/json": { "schema": { "$ref": "#/components/schemas/SetAccountState" } }
                    } },
                    "responses": {
                        "200": { "description": "The updated account", "content": {
                            "application/json": { "schema": { "$ref": "#/components/schemas/Account" } }
                        } },
                        "404": not_found,
                        "409": conflict,
                        "422": invalid
                    }
                }
            },
            "/status": {
                "get": {
                    "operationId": "accountStatus",
                    "summary": "Account counts by state",
                    "security": [{ "bearerAuth": [] }],
                    "responses": {
                        "200": { "description": "The counts", "content": {
                            "application/json": { "schema": { "$ref": "#/components/schemas/StatusSummary" } }
                        } }
                    }
                }
            },
            "/sessions": {
                "get": {
                    "operationId": "listSessions",
                    "summary": "Cursor page of the tenant's auth sessions (ordered by created_at, id)",
                    "security": [{ "bearerAuth": [] }],
                    "parameters": [
                        { "name": "cursor", "in": "query", "schema": { "type": "string" },
                          "description": "Opaque cursor from a previous page" },
                        { "name": "limit", "in": "query", "schema": { "type": "integer", "minimum": 1 },
                          "description": "Page size (default 50, cap 200)" }
                    ],
                    "responses": {
                        "200": { "description": "One page plus the next cursor", "content": {
                            "application/json": { "schema": { "$ref": "#/components/schemas/SessionPage" } }
                        } },
                        "400": invalid
                    }
                },
                "post": {
                    "operationId": "startSession",
                    "summary": "Start a login session (PENDING)",
                    "security": [{ "bearerAuth": [] }],
                    "requestBody": { "required": true, "content": {
                        "application/json": { "schema": { "$ref": "#/components/schemas/StartSession" } }
                    } },
                    "responses": {
                        "201": { "description": "The created session", "content": {
                            "application/json": { "schema": { "$ref": "#/components/schemas/AuthSession" } }
                        } },
                        "404": not_found,
                        "409": conflict,
                        "422": invalid
                    }
                }
            },
            "/sessions/{id}": {
                "get": {
                    "operationId": "getSession",
                    "summary": "One auth session",
                    "security": [{ "bearerAuth": [] }],
                    "parameters": [path_param],
                    "responses": {
                        "200": { "description": "The session", "content": {
                            "application/json": { "schema": { "$ref": "#/components/schemas/AuthSession" } }
                        } },
                        "404": not_found
                    }
                }
            },
            "/sessions/{id}/transition": {
                "post": {
                    "operationId": "transitionSession",
                    "summary": "One named state step (confirm/authorize/expire/revoke/fail/take-over/resume)",
                    "security": [{ "bearerAuth": [] }],
                    "parameters": [path_param],
                    "requestBody": { "required": true, "content": {
                        "application/json": { "schema": { "$ref": "#/components/schemas/TransitionSession" } }
                    } },
                    "responses": {
                        "200": { "description": "The updated session", "content": {
                            "application/json": { "schema": { "$ref": "#/components/schemas/AuthSession" } }
                        } },
                        "404": not_found,
                        "409": conflict,
                        "422": invalid
                    }
                }
            },
            "/events": {
                "get": {
                    "operationId": "listEvents",
                    "summary": "Bounded page of the tenant's event log (cursor pagination)",
                    "security": [{ "bearerAuth": [] }],
                    "parameters": [
                        { "name": "cursor", "in": "query", "schema": { "type": "string" },
                          "description": "Opaque cursor (the last delivered sequence number)" },
                        { "name": "limit", "in": "query", "schema": { "type": "integer", "minimum": 1 },
                          "description": "Maximum events in this page" }
                    ],
                    "responses": {
                        "200": { "description": "One page plus the next cursor", "content": {
                            "application/json": { "schema": { "$ref": "#/components/schemas/EventPage" } }
                        } }
                    }
                }
            },
            "/events/stream": {
                "get": {
                    "operationId": "streamEvents",
                    "summary": "SSE stream of the tenant's event log",
                    "description": "text/event-stream. Reconnect with `Last-Event-ID`. \
                                    A `slow-consumer` event names the `resume_from` id.",
                    "security": [{ "bearerAuth": [] }],
                    "responses": {
                        "200": { "description": "The event stream", "content": {
                            "text/event-stream": { "schema": { "type": "string" } }
                        } }
                    }
                }
            }
        },
        "components": {
            "securitySchemes": {
                "bearerAuth": { "type": "http", "scheme": "bearer",
                    "description": "API key bound to a tenant and scopes" }
            },
            "schemas": {
                "Problem": {
                    "type": "object",
                    "required": ["type", "title", "status"],
                    "properties": {
                        "type": { "type": "string", "format": "uri-reference" },
                        "title": { "type": "string" },
                        "status": { "type": "integer" },
                        "detail": { "type": "string" }
                    }
                },
                "Health": {
                    "type": "object",
                    "required": ["status", "service", "version"],
                    "properties": {
                        "status": { "type": "string" },
                        "service": { "type": "string" },
                        "version": { "type": "string" }
                    }
                },
                "AccountList": {
                    "type": "object",
                    "required": ["accounts"],
                    "properties": {
                        "accounts": { "type": "array", "items": { "type": "object" } }
                    }
                },
                "Account": {
                    "type": "object",
                    "required": ["id", "platform", "state", "updated_at"],
                    "properties": {
                        "id": { "type": "string" },
                        "platform": { "type": "string" },
                        "state": { "type": "string", "enum": ["ONLINE", "OFFLINE", "UNKNOWN", "DISABLED"] },
                        "state_note": { "type": "string" },
                        "updated_at": { "type": "integer", "description": "unix ms" }
                    }
                },
                "RegisterAccount": {
                    "type": "object",
                    "required": ["id", "platform"],
                    "properties": {
                        "id": { "type": "string" },
                        "platform": { "type": "string" }
                    }
                },
                "SetAccountState": {
                    "type": "object",
                    "required": ["state"],
                    "properties": {
                        "state": { "type": "string" },
                        "note": { "type": "string" }
                    }
                },
                "StatusSummary": {
                    "type": "object",
                    "required": ["online", "offline", "unknown", "disabled", "total"],
                    "properties": {
                        "online": { "type": "integer" },
                        "offline": { "type": "integer" },
                        "unknown": { "type": "integer" },
                        "disabled": { "type": "integer" },
                        "total": { "type": "integer" }
                    }
                },
                "AuthSession": {
                    "type": "object",
                    "required": ["id", "account_id", "platform", "state", "created_at", "updated_at"],
                    "properties": {
                        "id": { "type": "string" },
                        "account_id": { "type": "string" },
                        "platform": { "type": "string" },
                        "state": { "type": "string",
                            "enum": ["PENDING", "AWAITING_CONFIRMATION", "AUTHORIZED",
                                     "EXPIRED", "REVOKED", "FAILED", "TAKEN_OVER"] },
                        "state_note": { "type": "string" },
                        "credential_id": { "type": "string" },
                        "created_at": { "type": "integer", "description": "unix ms" },
                        "updated_at": { "type": "integer", "description": "unix ms" },
                        "expires_at": { "type": "integer", "description": "unix ms" }
                    }
                },
                "StartSession": {
                    "type": "object",
                    "required": ["id", "account_id"],
                    "properties": {
                        "id": { "type": "string", "description": "client-chosen, unique per tenant" },
                        "account_id": { "type": "string" },
                        "expires_at": { "type": "integer", "description": "unix ms" }
                    }
                },
                "TransitionSession": {
                    "type": "object",
                    "required": ["action"],
                    "properties": {
                        "action": { "type": "string",
                            "enum": ["confirm", "authorize", "expire", "revoke", "fail", "take-over", "resume"] },
                        "note": { "type": "string" },
                        "credential_id": { "type": "string", "description": "authorize only" },
                        "credential_version": { "type": "integer", "description": "authorize only" },
                        "expires_at": { "type": "integer", "description": "authorize only" }
                    }
                },
                "SessionPage": {
                    "type": "object",
                    "required": ["sessions", "next_cursor"],
                    "properties": {
                        "sessions": { "type": "array", "items": { "type": "object" } },
                        "next_cursor": { "type": ["string", "null"] }
                    }
                },
                "EventPage": {
                    "type": "object",
                    "required": ["events", "next_cursor"],
                    "properties": {
                        "events": { "type": "array", "items": { "type": "object" } },
                        "next_cursor": { "type": ["string", "null"] }
                    }
                }
            }
        }
    })
}

/// Serves the document at `/openapi.json`.
pub async fn serve() -> Json<serde_json::Value> {
    Json(document())
}

/// Test helper: the problem-type base, shared with contract tests.
#[allow(dead_code)]
pub fn problem_base() -> &'static str {
    PROBLEM_BASE
}
