"""F036: contract tests for the generated Python SDK.

The client is generated; these tests pin its behavior — bearer
injection, the X-Takeover-Token possession path, idempotency, cursor
queries, the SSE drain, and RFC 9457 classification — through the
`transport` test seam, with no network.
"""

from __future__ import annotations

import json
import unittest
from urllib.parse import quote, urlparse

from fishpond import FishpondClient, FishpondError, classify_problem

PROBLEM_BASE = "https://fishpond.dev/problems/"


def json_response(payload: dict, status: int = 200):
    return status, json.dumps(payload).encode()


class Recorder:
    """Transport seam that records every call and delegates responses."""

    def __init__(self, responder=None) -> None:
        self.calls: list[dict] = []
        self.responder = responder or (lambda call: json_response({}))

    def __call__(self, method, url, headers, body):
        call = {
            "method": method,
            "url": url,
            "headers": dict(headers),
            "body": body,
        }
        self.calls.append(call)
        return self.responder(call)

    def last(self) -> dict:
        assert self.calls, "no call recorded"
        return self.calls[-1]


class ClientTestCase(unittest.TestCase):
    def setUp(self) -> None:
        self.transport = Recorder()
        self.client = FishpondClient(
            "http://control.test", key=lambda: "key-1", transport=self.transport
        )

    def authorized(self, call: dict) -> str | None:
        return {
            k.lower(): v for k, v in call["headers"].items()
        }.get("authorization")


class TransportCore(ClientTestCase):
    def test_bearer_injected_and_base_url_joined(self) -> None:
        self.transport.responder = lambda call: json_response(
            {"id": "acc-1", "platform": "qq", "state": "ONLINE", "updated_at": 1}
        )
        account = self.client.get_account("acc-1")
        self.assertEqual(account["id"], "acc-1")
        self.assertEqual(self.transport.last()["method"], "GET")
        self.assertEqual(self.transport.last()["url"], "http://control.test/accounts/acc-1")
        self.assertEqual(self.authorized(self.transport.last()), "Bearer key-1")

    def test_trailing_slash_stripped(self) -> None:
        client = FishpondClient(
            "http://control.test/", key=lambda: "k", transport=self.transport
        )
        client.health()
        self.assertEqual(self.transport.last()["url"], "http://control.test/healthz")

    def test_no_bearer_without_key(self) -> None:
        anonymous = FishpondClient("http://control.test", transport=self.transport)
        anonymous.list_accounts()
        self.assertIsNone(self.authorized(self.transport.last()))

    def test_no_bearer_for_empty_key(self) -> None:
        empty = FishpondClient(
            "http://control.test", key=lambda: "", transport=self.transport
        )
        empty.list_accounts()
        self.assertIsNone(self.authorized(self.transport.last()))

    def test_body_sent_as_json(self) -> None:
        self.client.register_account({"id": "acc-9", "platform": "qq"})
        call = self.transport.last()
        self.assertEqual(call["method"], "POST")
        self.assertEqual(call["headers"].get("content-type"), "application/json")
        self.assertEqual(json.loads(call["body"]), {"id": "acc-9", "platform": "qq"})

    def test_path_arguments_are_percent_encoded(self) -> None:
        self.client.get_account("a b/c")
        self.assertEqual(
            self.transport.last()["url"],
            "http://control.test/accounts/" + quote("a b/c", safe=""),
        )


class AnonymousPaths(ClientTestCase):
    def test_health_never_sends_bearer(self) -> None:
        self.client.health()
        self.assertIsNone(self.authorized(self.transport.last()))

    def test_verify_takeover_is_possession_only(self) -> None:
        self.transport.responder = lambda call: (
            403,
            json.dumps(
                {
                    "type": PROBLEM_BASE + "scope-denied",
                    "title": "forbidden",
                    "status": 403,
                    "detail": "scope missing",
                }
            ).encode(),
        )
        with self.assertRaises(FishpondError) as caught:
            self.client.verify_takeover("tok-secret")
        call = self.transport.last()
        self.assertEqual(call["method"], "POST")
        self.assertEqual(call["url"], "http://control.test/takeover/verify")
        self.assertEqual(call["headers"].get("X-Takeover-Token"), "tok-secret")
        self.assertIsNone(self.authorized(call))
        self.assertEqual(caught.exception.kind, "forbidden")
        self.assertEqual(caught.exception.status, 403)


class Queries(ClientTestCase):
    def test_cursor_and_limit_encoded(self) -> None:
        self.client.list_sessions("c3", 5)
        parsed = urlparse(self.transport.last()["url"])
        self.assertEqual(parsed.path, "/sessions")
        self.assertEqual(parsed.query, "cursor=c3&limit=5")

    def test_none_query_arguments_omitted(self) -> None:
        self.client.list_events()
        self.assertEqual(urlparse(self.transport.last()["url"]).query, "")

    def test_stream_events_drains_text(self) -> None:
        self.transport.responder = lambda call: (200, b"event: caught-up\ndata: {}\n")
        text = self.client.stream_events(consumer="worker-1", last_event_id="e42")
        self.assertEqual(text, "event: caught-up\ndata: {}\n")
        call = self.transport.last()
        headers = {k.lower(): v for k, v in call["headers"].items()}
        self.assertEqual(headers.get("accept"), "text/event-stream")
        self.assertEqual(headers.get("last-event-id"), "e42")
        self.assertEqual(urlparse(call["url"]).query, "consumer=worker-1")


class IdempotentOperations(ClientTestCase):
    def test_create_operation_requires_idempotency_header(self) -> None:
        self.client.create_operation(
            "idem-7", {"id": "op-1", "kind": "send_message"}
        )
        call = self.transport.last()
        self.assertEqual(call["method"], "POST")
        self.assertEqual(call["headers"].get("Idempotency-Key"), "idem-7")
        self.assertEqual(
            json.loads(call["body"]), {"id": "op-1", "kind": "send_message"}
        )

    def test_close_attempt_posts_to_nested_path(self) -> None:
        self.client.close_attempt(
            "op-1", "at-1", {"worker_id": "w-1", "result": "succeeded"}
        )
        self.assertEqual(
            self.transport.last()["url"],
            "http://control.test/operations/op-1/attempts/at-1/close",
        )


class ProblemClassification(ClientTestCase):
    SLUGS = {
        "forbidden": "forbidden",
        "scope-denied": "forbidden",
        "not-found": "missing",
        "invalid": "invalid",
        "invalid-cursor": "invalid",
        "conflict": "conflict",
        "unauthorized": "unauthorized",
        "reauth-required": "reauth",
        "store-refused": "server",
        "storage": "server",
    }

    def problem(self, slug: str, status: int):
        self.transport.responder = lambda call: (
            status,
            json.dumps(
                {
                    "type": PROBLEM_BASE + slug,
                    "title": "title text",
                    "status": status,
                    "detail": "detail text",
                }
            ).encode(),
        )

    def test_every_slug_classifies(self) -> None:
        for slug, kind in self.SLUGS.items():
            with self.subTest(slug=slug):
                self.problem(slug, 400)
                with self.assertRaises(FishpondError) as caught:
                    self.client.get_account("acc-1")
                self.assertEqual(caught.exception.kind, kind)
                self.assertEqual(
                    caught.exception.problem["type"], PROBLEM_BASE + slug
                )

    def test_detail_becomes_the_message(self) -> None:
        self.problem("not-found", 404)
        with self.assertRaises(FishpondError) as caught:
            self.client.get_account("acc-1")
        self.assertIn("detail text", str(caught.exception))

    def test_foreign_uri_is_other(self) -> None:
        self.transport.responder = lambda call: (
            400,
            json.dumps(
                {"type": "https://else.example/x", "title": "t", "status": 400}
            ).encode(),
        )
        with self.assertRaises(FishpondError) as caught:
            self.client.list_accounts()
        self.assertEqual(caught.exception.kind, "other")

    def test_non_problem_body_becomes_store_refused(self) -> None:
        self.transport.responder = lambda call: (502, b"<html>boom</html>")
        with self.assertRaises(FishpondError) as caught:
            self.client.list_accounts()
        self.assertEqual(caught.exception.kind, "server")
        self.assertEqual(
            caught.exception.problem["type"], PROBLEM_BASE + "store-refused"
        )
        self.assertEqual(caught.exception.problem["status"], 502)

    def test_reference_classifier(self) -> None:
        for slug, kind in self.SLUGS.items():
            self.assertEqual(classify_problem(PROBLEM_BASE + slug), kind, slug)
        self.assertEqual(classify_problem("https://else.example/x"), "other")
        self.assertEqual(classify_problem(PROBLEM_BASE + "nope"), "other")


class PageParsing(ClientTestCase):
    def test_session_page_dict_passthrough(self) -> None:
        self.transport.responder = lambda call: json_response(
            {
                "sessions": [
                    {
                        "id": "s-1",
                        "account_id": "acc-1",
                        "platform": "qq",
                        "state": "PENDING",
                        "created_at": 1,
                        "updated_at": 1,
                    }
                ],
                "next_cursor": None,
            }
        )
        page = self.client.list_sessions()
        self.assertEqual(page["sessions"][0]["state"], "PENDING")
        self.assertIsNone(page["next_cursor"])

    def test_status_summary(self) -> None:
        self.transport.responder = lambda call: json_response(
            {"total": 3, "online": 1, "offline": 1, "unknown": 1, "disabled": 0}
        )
        summary = self.client.account_status()
        self.assertEqual(summary["total"], 3)


if __name__ == "__main__":
    unittest.main()
