"""fishpond control-plane SDK.

GENERATED FILE — scripts/generate_sdk.py from crates/server/tests/fixtures/openapi.json. Do not edit by hand.
"""
from __future__ import annotations

import json
import urllib.error
import urllib.request
from urllib.parse import quote, urlencode

PROBLEM_BASE = "https://fishpond.dev/problems/"

PROBLEM_KINDS = {
    "conflict": "conflict",
    "forbidden": "forbidden",
    "invalid": "invalid",
    "invalid-cursor": "invalid",
    "not-found": "missing",
    "reauth-required": "reauth",
    "scope-denied": "forbidden",
    "storage": "server",
    "store-refused": "server",
    "unauthorized": "unauthorized",
}


def classify_problem(type_uri: str) -> str:
    """Maps a problem type URI to the SDK's failure classes."""
    if not type_uri.startswith(PROBLEM_BASE):
        return "other"
    return PROBLEM_KINDS.get(type_uri.rsplit("/", 1)[-1], "other")


class FishpondError(Exception):
    """One failed call: the problem document plus its class."""

    def __init__(self, problem: dict, kind: str, status: int) -> None:
        self.problem = problem
        self.kind = kind
        self.status = status
        detail = problem.get("detail") or problem.get("title")
        super().__init__(f"{kind}: {detail}")


class FishpondClient:
    """Typed client for the fishpond control plane.

    `transport` is a test seam: a callable
    (method, url, headers, body) -> (status, payload_bytes).
    """

    def __init__(self, base_url: str, key=None, transport=None) -> None:
        self.base_url = base_url.rstrip("/")
        self._key = key
        self._transport = transport

    def _request(
        self,
        method: str,
        path: str,
        *,
        path_args=None,
        headers=None,
        body=None,
        bearer=True,
        raw=False,
        query=None,
    ):
        for name, value in (path_args or {}).items():
            path = path.replace("{" + name + "}", quote(str(value), safe=""))
        clean = {k: v for k, v in (query or {}).items() if v is not None}
        if clean:
            path = path + "?" + urlencode(clean)
        sent = dict(headers or {})
        key = self._key() if callable(self._key) else self._key
        if bearer and key:
            sent["authorization"] = f"Bearer {key}"
        data = None
        if body is not None:
            sent["content-type"] = "application/json"
            data = json.dumps(body).encode()
        if raw:
            sent["accept"] = "text/event-stream"
        url = self.base_url + path
        if self._transport is not None:
            status, payload = self._transport(method, url, sent, data)
        else:
            request = urllib.request.Request(url, data=data, method=method, headers=sent)
            try:
                with urllib.request.urlopen(request) as response:
                    status = response.status
                    payload = response.read()
            except urllib.error.HTTPError as err:
                status = err.code
                payload = err.read()
        if status >= 300:
            try:
                problem = json.loads(payload)
            except Exception:
                problem = {
                    "type": PROBLEM_BASE + "store-refused",
                    "title": "error",
                    "status": status,
                    "detail": f"control plane answered {status} with a non-problem body",
                }
            raise FishpondError(problem, classify_problem(problem.get("type", "")), status)
        if raw:
            return payload.decode()
        if not payload:
            return None
        return json.loads(payload)


    def account_status(self, ) -> StatusSummary:
        """Account counts by state"""
        return self._request(
            "GET",
            "/status",
        )


    def claim_operation(self, id, body: dict) -> ClaimedOperation:
        """Claim a PENDING operation (CAS to RUNNING with a numbered attempt)

        Request body schema: ClaimOperation."""
        return self._request(
            "POST",
            "/operations/{id}/claim",
            path_args={"id": id},
            body=body,
        )


    def close_attempt(self, id, attempt_id, body: dict) -> Operation:
        """Settle one attempt (succeeded/failed end the operation; timed-out requeues)

        Request body schema: CloseAttempt."""
        return self._request(
            "POST",
            "/operations/{id}/attempts/{attempt_id}/close",
            path_args={"id": id, "attempt_id": attempt_id},
            body=body,
        )


    def create_operation(self, idempotency_key, body: dict) -> CreatedOperation:
        """Idempotently create an operation (PENDING)

        Request body schema: CreateOperation."""
        headers = {"Idempotency-Key": idempotency_key}
        return self._request(
            "POST",
            "/operations",
            headers=headers,
            body=body,
        )


    def delete_account(self, id) -> DeletedAccount:
        """Delete an account with its sessions, conversations, messages, grants, leases and profiles (observation history survives as audit trail)"""
        return self._request(
            "DELETE",
            "/accounts/{id}",
            path_args={"id": id},
        )


    def get_account(self, id) -> Account:
        """One account"""
        return self._request(
            "GET",
            "/accounts/{id}",
            path_args={"id": id},
        )


    def get_operation(self, id) -> Operation:
        """One operation"""
        return self._request(
            "GET",
            "/operations/{id}",
            path_args={"id": id},
        )


    def get_session(self, id) -> AuthSession:
        """One auth session"""
        return self._request(
            "GET",
            "/sessions/{id}",
            path_args={"id": id},
        )


    def health(self, ) -> Health:
        """Liveness probe"""
        return self._request(
            "GET",
            "/healthz",
            bearer=False,
        )


    def list_accounts(self, ) -> AccountList:
        """List the tenant's accounts (ordered by id)"""
        return self._request(
            "GET",
            "/accounts",
        )


    def list_events(self, cursor: str | None = None, limit: int | None = None) -> EventPage:
        """Bounded page of the tenant's event log (cursor pagination)"""
        return self._request(
            "GET",
            "/events",
            query={"cursor": cursor, "limit": limit},
        )


    def list_operations(self, cursor: str | None = None, limit: int | None = None) -> OperationPage:
        """List the tenant's operations (oldest first, cursor pagination)"""
        return self._request(
            "GET",
            "/operations",
            query={"cursor": cursor, "limit": limit},
        )


    def list_sessions(self, cursor: str | None = None, limit: int | None = None) -> SessionPage:
        """Cursor page of the tenant's auth sessions (ordered by created_at, id)"""
        return self._request(
            "GET",
            "/sessions",
            query={"cursor": cursor, "limit": limit},
        )


    def list_takeover_grants(self, id) -> GrantList:
        """The session's takeover grant audit list (hashes and hints, never raw tokens)"""
        return self._request(
            "GET",
            "/sessions/{id}/takeover",
            path_args={"id": id},
        )


    def register_account(self, body: dict) -> Account:
        """Register an account (OFFLINE)

        Request body schema: RegisterAccount."""
        return self._request(
            "POST",
            "/accounts",
            body=body,
        )


    def release_takeover(self, id, grant_id) -> ReleaseResult:
        """Release the takeover: the credential dies and automation resumes"""
        return self._request(
            "DELETE",
            "/sessions/{id}/takeover/{grant_id}",
            path_args={"id": id, "grant_id": grant_id},
        )


    def set_account_state(self, id, body: dict) -> Account:
        """One account state-machine step

        Request body schema: SetAccountState."""
        return self._request(
            "POST",
            "/accounts/{id}/state",
            path_args={"id": id},
            body=body,
        )


    def start_session(self, body: dict) -> AuthSession:
        """Start a login session (PENDING)

        Request body schema: StartSession."""
        return self._request(
            "POST",
            "/sessions",
            body=body,
        )


    def start_takeover(self, id, body: dict) -> GrantIssued:
        """Take the session over and mint the operator credential (the raw token appears exactly once)

        Request body schema: StartTakeover."""
        return self._request(
            "POST",
            "/sessions/{id}/takeover",
            path_args={"id": id},
            body=body,
        )


    def stream_events(self, consumer: str | None = None, last_event_id: str | None = None) -> str:
        """SSE stream of the tenant's event log"""
        headers = {}
        if last_event_id is not None:
            headers["Last-Event-ID"] = last_event_id
        return self._request(
            "GET",
            "/events/stream",
            headers=headers,
            raw=True,
            query={"consumer": consumer},
        )


    def transition_session(self, id, body: dict) -> AuthSession:
        """One named state step (confirm/authorize/expire/revoke/fail/take-over/resume)

        Request body schema: TransitionSession."""
        return self._request(
            "POST",
            "/sessions/{id}/transition",
            path_args={"id": id},
            body=body,
        )


    def verify_takeover(self, x_takeover_token) -> TakeoverContext:
        """Verify an operator takeover token by possession (X-Takeover-Token header; no bearer key)"""
        headers = {"X-Takeover-Token": x_takeover_token}
        return self._request(
            "POST",
            "/takeover/verify",
            headers=headers,
            bearer=False,
        )


