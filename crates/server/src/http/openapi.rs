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
                    "summary": "List the tenant's accounts",
                    "security": [{ "bearerAuth": [] }],
                    "responses": {
                        "200": { "description": "The account list", "content": {
                            "application/json": { "schema": { "$ref": "#/components/schemas/AccountList" } }
                        } },
                        "401": { "description": "Missing or unknown key", "content": {
                            "application/problem+json": { "schema": problem_ref }
                        } }
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
                          "description": "Opaque cursor from a previous page" },
                        { "name": "limit", "in": "query", "schema": { "type": "integer", "minimum": 1 },
                          "description": "Maximum events in this page" }
                    ],
                    "responses": {
                        "200": { "description": "One page plus the next cursor", "content": {
                            "application/json": { "schema": { "$ref": "#/components/schemas/EventPage" } }
                        } },
                        "401": { "description": "Missing or unknown key", "content": {
                            "application/problem+json": { "schema": problem_ref }
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
                        } },
                        "401": { "description": "Missing or unknown key", "content": {
                            "application/problem+json": { "schema": problem_ref }
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
