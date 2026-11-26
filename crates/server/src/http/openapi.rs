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
            "/sessions/{id}/takeover": {
                "post": {
                    "operationId": "startTakeover",
                    "summary": "Take the session over and mint the operator credential (the raw token appears exactly once)",
                    "security": [{ "bearerAuth": [] }],
                    "parameters": [path_param],
                    "requestBody": { "required": true, "content": {
                        "application/json": { "schema": { "type": "object", "required": ["operator"], "properties": {
                            "operator": { "type": "string", "description": "Who the credential is for (audit display)" },
                            "ttl_ms": { "type": "integer", "description": "Requested life in ms; defaults to the service cap" }
                        } } }
                    } },
                    "responses": {
                        "201": { "description": "The minted credential", "content": {
                            "application/json": { "schema": { "type": "object", "required": ["grant_id", "token", "expires_at"], "properties": {
                                "grant_id": { "type": "string" },
                                "token": { "type": "string", "description": "Raw token; store it now — the server only holds a hash" },
                                "expires_at": { "type": "integer", "format": "int64" }
                            } } }
                        } },
                        "403": scope_denied,
                        "404": not_found,
                        "409": conflict,
                        "422": invalid
                    }
                },
                "get": {
                    "operationId": "listTakeoverGrants",
                    "summary": "The session's takeover grant audit list (hashes and hints, never raw tokens)",
                    "security": [{ "bearerAuth": [] }],
                    "parameters": [path_param],
                    "responses": {
                        "200": { "description": "The grants", "content": {
                            "application/json": { "schema": { "type": "object", "required": ["grants"], "properties": {
                                "grants": { "type": "array", "items": { "type": "object" } }
                            } } }
                        } },
                        "403": scope_denied,
                        "404": not_found
                    }
                }
            },
            "/sessions/{id}/takeover/{grant_id}": {
                "delete": {
                    "operationId": "releaseTakeover",
                    "summary": "Release the takeover: the credential dies and automation resumes",
                    "security": [{ "bearerAuth": [] }],
                    "parameters": [path_param,
                        { "name": "grant_id", "in": "path", "required": true, "schema": { "type": "string" } }],
                    "responses": {
                        "200": { "description": "Released", "content": {
                            "application/json": { "schema": { "type": "object", "required": ["released"], "properties": {
                                "released": { "type": "boolean" }
                            } } }
                        } },
                        "403": scope_denied,
                        "404": not_found
                    }
                }
            },
            "/takeover/verify": {
                "post": {
                    "operationId": "verifyTakeover",
                    "summary": "Verify an operator takeover token by possession (X-Takeover-Token header; no bearer key)",
                    "parameters": [
                        { "name": "X-Takeover-Token", "in": "header", "required": true,
                          "schema": { "type": "string" },
                          "description": "The operator takeover token (`fp_tk_…`); \
                                          possession of the token is the credential" }
                    ],
                    "responses": {
                        "200": { "description": "What the token proves", "content": {
                            "application/json": { "schema": { "type": "object", "required": ["grant_id", "tenant", "account", "session", "operator", "expires_at"], "properties": {
                                "grant_id": { "type": "string" },
                                "tenant": { "type": "string" },
                                "account": { "type": "string" },
                                "session": { "type": "string" },
                                "operator": { "type": "string" },
                                "expires_at": { "type": "integer", "format": "int64" }
                            } } }
                        } },
                        "401": unauthorized,
                        "404": not_found
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
                    "description": "text/event-stream. Drains to the current head, then ends \
                                    with a terminal `caught-up` event; reconnect (optionally \
                                    with `Last-Event-ID`) for more. A `slow-consumer` event \
                                    names the `resume_from` id to reconnect with.",
                    "security": [{ "bearerAuth": [] }],
                    "parameters": [
                        { "name": "Last-Event-ID", "in": "header", "schema": { "type": "string" },
                          "description": "Replay strictly after this sequence number" },
                        { "name": "consumer", "in": "query", "schema": { "type": "string" },
                          "description": "Consumer name for the persisted checkpoint" }
                    ],
                    "responses": {
                        "200": { "description": "The event stream", "content": {
                            "text/event-stream": { "schema": { "type": "string" } }
                        } }
                    }
                }
            },
            "/operations": {
                "get": {
                    "operationId": "listOperations",
                    "summary": "List the tenant's operations (oldest first, cursor pagination)",
                    "security": [{ "bearerAuth": [] }],
                    "parameters": [
                        { "name": "cursor", "in": "query", "schema": { "type": "string" },
                          "description": "Opaque cursor" },
                        { "name": "limit", "in": "query", "schema": { "type": "integer", "minimum": 1 },
                          "description": "Maximum operations in this page" }
                    ],
                    "responses": {
                        "200": { "description": "One page plus the next cursor", "content": {
                            "application/json": { "schema": { "$ref": "#/components/schemas/OperationPage" } }
                        } },
                        "401": unauthorized,
                        "403": scope_denied
                    }
                },
                "post": {
                    "operationId": "createOperation",
                    "summary": "Idempotently create an operation (PENDING)",
                    "description": "The `Idempotency-Key` header is mandatory. Replaying the \
                                    same key with the same id+kind returns the original \
                                    operation with 200; the same key with different work is \
                                    a 409.",
                    "security": [{ "bearerAuth": [] }],
                    "parameters": [
                        { "name": "Idempotency-Key", "in": "header", "required": true,
                          "schema": { "type": "string" } }
                    ],
                    "requestBody": { "required": true, "content": {
                        "application/json": { "schema": { "$ref": "#/components/schemas/CreateOperation" } }
                    } },
                    "responses": {
                        "201": { "description": "The created operation", "content": {
                            "application/json": { "schema": { "$ref": "#/components/schemas/CreatedOperation" } }
                        } },
                        "200": { "description": "Idempotent replay of the original operation", "content": {
                            "application/json": { "schema": { "$ref": "#/components/schemas/CreatedOperation" } }
                        } },
                        "400": invalid,
                        "401": unauthorized,
                        "403": scope_denied,
                        "409": conflict,
                        "422": invalid
                    }
                }
            },
            "/operations/{id}": {
                "get": {
                    "operationId": "getOperation",
                    "summary": "One operation",
                    "security": [{ "bearerAuth": [] }],
                    "parameters": [path_param],
                    "responses": {
                        "200": { "description": "The operation", "content": {
                            "application/json": { "schema": { "$ref": "#/components/schemas/Operation" } }
                        } },
                        "401": unauthorized,
                        "403": scope_denied,
                        "404": not_found
                    }
                }
            },
            "/operations/{id}/claim": {
                "post": {
                    "operationId": "claimOperation",
                    "summary": "Claim a PENDING operation (CAS to RUNNING with a numbered attempt)",
                    "security": [{ "bearerAuth": [] }],
                    "parameters": [path_param],
                    "requestBody": { "required": true, "content": {
                        "application/json": { "schema": { "$ref": "#/components/schemas/ClaimOperation" } }
                    } },
                    "responses": {
                        "200": { "description": "The claimed operation and its attempt", "content": {
                            "application/json": { "schema": { "$ref": "#/components/schemas/ClaimedOperation" } }
                        } },
                        "401": unauthorized,
                        "403": scope_denied,
                        "404": not_found,
                        "409": conflict
                    }
                }
            },
            "/operations/{id}/attempts/{attempt_id}/close": {
                "post": {
                    "operationId": "closeAttempt",
                    "summary": "Settle one attempt (succeeded/failed end the operation; timed-out requeues)",
                    "security": [{ "bearerAuth": [] }],
                    "parameters": [
                        path_param,
                        { "name": "attempt_id", "in": "path", "required": true,
                          "schema": { "type": "string" }, "description": "Attempt id" }
                    ],
                    "requestBody": { "required": true, "content": {
                        "application/json": { "schema": { "$ref": "#/components/schemas/CloseAttempt" } }
                    } },
                    "responses": {
                        "200": { "description": "The operation after the close", "content": {
                            "application/json": { "schema": { "$ref": "#/components/schemas/Operation" } }
                        } },
                        "401": unauthorized,
                        "403": scope_denied,
                        "404": not_found,
                        "409": conflict,
                        "422": invalid
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
                },
                "Operation": {
                    "type": "object",
                    "required": ["id", "kind", "state", "created_at"],
                    "properties": {
                        "id": { "type": "string" },
                        "kind": { "type": "string" },
                        "state": { "type": "string",
                            "enum": ["PENDING", "RUNNING", "SUCCEEDED", "FAILED", "CANCELLED"] },
                        "created_at": { "type": "integer", "description": "unix ms" }
                    }
                },
                "CreateOperation": {
                    "type": "object",
                    "required": ["id", "kind"],
                    "properties": {
                        "id": { "type": "string", "description": "client-chosen, unique per tenant" },
                        "kind": { "type": "string", "description": "what to do (e.g. send_message)" }
                    }
                },
                "CreatedOperation": {
                    "type": "object",
                    "required": ["operation", "replayed"],
                    "properties": {
                        "operation": { "$ref": "#/components/schemas/Operation" },
                        "replayed": { "type": "boolean",
                            "description": "true when the key returned the original, not new work" }
                    }
                },
                "ClaimOperation": {
                    "type": "object",
                    "required": ["worker_id", "attempt_id", "deadline_at"],
                    "properties": {
                        "worker_id": { "type": "string" },
                        "attempt_id": { "type": "string", "description": "client-chosen, unique per attempt" },
                        "deadline_at": { "type": "integer", "description": "attempt deadline, unix ms" }
                    }
                },
                "ClaimedOperation": {
                    "type": "object",
                    "required": ["operation", "attempt"],
                    "properties": {
                        "operation": { "$ref": "#/components/schemas/Operation" },
                        "attempt": { "type": "object",
                            "required": ["attempt_id", "attempt_no", "deadline_at"],
                            "properties": {
                                "attempt_id": { "type": "string" },
                                "attempt_no": { "type": "integer" },
                                "deadline_at": { "type": "integer", "description": "unix ms" }
                            } }
                    }
                },
                "CloseAttempt": {
                    "type": "object",
                    "required": ["worker_id", "result"],
                    "properties": {
                        "worker_id": { "type": "string" },
                        "result": { "type": "string", "enum": ["succeeded", "failed", "timed-out"] }
                    }
                },
                "OperationPage": {
                    "type": "object",
                    "required": ["operations", "next_cursor"],
                    "properties": {
                        "operations": { "type": "array", "items": { "type": "object" } },
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
