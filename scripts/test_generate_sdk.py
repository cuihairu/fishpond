"""F036 generator tests: the SDKs are faithful, deterministic projections
of the contract fixture. Run with `python3 -m unittest discover -s scripts`.
"""

from __future__ import annotations

import json
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

import generate_sdk as gen  # noqa: E402

REPO = Path(__file__).resolve().parent.parent
FIXTURE = REPO / "crates/server/tests/fixtures/openapi.json"


def load() -> tuple[dict, list[gen.Operation]]:
    doc = json.loads(FIXTURE.read_text())
    return doc, gen.analyze(doc)


DOC, OPS = load()
BY_ID = {op.op_id: op for op in OPS}


def ts_out() -> str:
    return gen.emit_ts(DOC, OPS)


def py_out() -> str:
    return gen.emit_py(DOC, OPS)


def go_out() -> str:
    return gen.emit_go(DOC, OPS)


class Determinism(unittest.TestCase):
    """The same document must always render to the same bytes."""

    def test_all_three_outputs_are_byte_stable(self):
        for render in (ts_out, py_out, go_out):
            self.assertEqual(render(), render(), render.__name__)

    def test_main_writes_identical_files_on_reruns(self):
        with tempfile.TemporaryDirectory() as tmp:
            # render twice into two directories, byte-compare
            for target_dir in ("run1", "run2"):
                doc = json.loads(FIXTURE.read_text())
                ops = gen.analyze(doc)
                base = f"{tmp}/{target_dir}"
                for text, name in (
                    (gen.emit_ts(doc, ops), "c.ts"),
                    (gen.emit_py(doc, ops), "c.py"),
                    (gen.emit_go(doc, ops), "c.go"),
                    (gen.GOMOD, "go.mod"),
                ):
                    (Path(base) / name).parent.mkdir(parents=True, exist_ok=True)
                    (Path(base) / name).write_text(text)
            for name in ("c.ts", "c.py", "c.go", "go.mod"):
                first = (Path(tmp) / "run1" / name).read_bytes()
                second = (Path(tmp) / "run2" / name).read_bytes()
                self.assertEqual(first, second, name)

    def test_generated_files_current_in_repo(self):
        """The committed artifacts match what the fixture renders right now."""
        checks = (
            (ts_out(), REPO / "packages/sdk/src/client.ts"),
            (py_out(), REPO / "sdk/python/fishpond/__init__.py"),
            (go_out(), REPO / "sdk/go/client.go"),
        )
        for rendered, path in checks:
            self.assertTrue(path.exists(), f"{path} missing")
            self.assertEqual(
                path.read_text(),
                rendered,
                f"{path} drifted; rerun scripts/generate_sdk.py",
            )


class OperationCoverage(unittest.TestCase):
    """Every contract operation lands in all three SDKs, correctly named."""

    def test_every_operation_is_analyzed(self):
        expected = {
            "listAccounts",
            "registerAccount",
            "getAccount",
            "setAccountState",
            "listEvents",
            "streamEvents",
            "health",
            "listOperations",
            "createOperation",
            "getOperation",
            "closeAttempt",
            "claimOperation",
            "listSessions",
            "startSession",
            "getSession",
            "listTakeoverGrants",
            "startTakeover",
            "releaseTakeover",
            "transitionSession",
            "accountStatus",
            "verifyTakeover",
        }
        self.assertEqual({op.op_id for op in OPS}, expected)

    def test_operation_names_in_all_outputs(self):
        ts, py, go = ts_out(), py_out(), go_out()
        for op in OPS:
            self.assertIn(f"async {op.op_id}(", ts, op.op_id)
            self.assertIn(f"def {gen.snake(op.op_id)}(self", py, op.op_id)
            self.assertIn(f"func (c *Client) {gen.pascal(op.op_id)}(", go, op.op_id)

    def test_path_params_become_typed_arguments(self):
        ts, py, go = ts_out(), py_out(), go_out()
        self.assertIn("async getAccount(id: string)", ts)
        self.assertIn("async releaseTakeover(id: string, grantId: string)", ts)
        self.assertIn("def get_account(self, id)", py)
        self.assertIn("path_args={\"id\": id}", py)
        self.assertIn("GetAccount(ctx context.Context, id string)", go)

    def test_query_params_are_optional_and_wired(self):
        ts, py, go = ts_out(), py_out(), go_out()
        self.assertIn("async listEvents(cursor?: string, limit?: number)", ts)
        self.assertIn("def list_events(self, cursor: str | None = None, limit: int | None = None)", py)
        self.assertIn("query={\"cursor\": cursor, \"limit\": limit}", py)
        self.assertIn("ListEvents(ctx context.Context, cursor *string, limit *int64)", go)
        self.assertIn("strconv.FormatInt(*limit, 10)", go)

    def test_schema_types_surface_as_return_types(self):
        ts, go = ts_out(), go_out()
        self.assertIn("async listAccounts(): Promise<AccountList>", ts)
        self.assertIn("async getAccount(id: string): Promise<Account>", ts)
        self.assertIn("ListAccounts(ctx context.Context) (AccountList, error)", go)

    def test_body_schemas_become_typed_arguments(self):
        ts, py, go = ts_out(), py_out(), go_out()
        self.assertIn("async startSession(body: StartSession)", ts)
        self.assertIn("def start_session(self, body: dict)", py)
        self.assertIn("Request body schema: StartSession.", py)
        self.assertIn("StartSession(ctx context.Context, body StartSession)", go)


class NamedSchemas(unittest.TestCase):
    """Contract schemas are typed, not bare objects."""

    def test_every_named_schema_is_a_ts_interface_and_go_struct(self):
        ts, go = ts_out(), go_out()
        for name in sorted(DOC["components"]["schemas"]):
            if name == "Problem":
                continue  # handwritten canonical shape in both SDKs
            self.assertIn(f"export interface {name} {{", ts, name)
            self.assertIn(f"type {name} struct {{", go, name)

    def test_inline_attempt_schema_gets_a_named_go_struct(self):
        self.assertIn("type ClaimedOperationAttempt struct {", go_out())

    def test_problem_interface_is_handwritten_and_stable(self):
        ts = ts_out()
        self.assertIn("export interface Problem {", ts)
        self.assertIn("https://fishpond.dev/problems/store-refused", ts)


class ProblemClassification(unittest.TestCase):
    """RFC 9457 slugs classify identically in all three SDKs."""

    def test_every_slug_present_in_all_outputs(self):
        ts, py, go = ts_out(), py_out(), go_out()
        for slug in gen.PROBLEM_KINDS:
            self.assertIn(f'"{slug}"', ts, slug)
            self.assertIn(f'"{slug}"', py, slug)
            self.assertIn(f'case "{slug}"', go, slug)

    def test_kind_set_matches_the_ten_classes(self):
        kinds = set(gen.PROBLEM_KINDS.values())
        self.assertEqual(
            kinds,
            {
                "forbidden",
                "missing",
                "invalid",
                "conflict",
                "unauthorized",
                "reauth",
                "server",
            },
        )
        self.assertEqual(len(gen.PROBLEM_KINDS), 10)

    def test_reference_classifier_matches_the_emitted_map(self):
        for slug, kind in gen.PROBLEM_KINDS.items():
            self.assertEqual(gen.classify_problem(gen.PROBLEM_BASE + slug), kind, slug)
        self.assertEqual(gen.classify_problem("https://elsewhere.example/x"), "other")
        self.assertEqual(gen.classify_problem(gen.PROBLEM_BASE + "nope"), "other")


class SecuritySemantics(unittest.TestCase):
    """Bearer injection, the possession path, and idempotency survive generation."""

    def test_anonymous_operations_are_exactly_health_and_verify(self):
        self.assertEqual(
            {op.op_id for op in OPS if not op.needs_bearer},
            {"health", "verifyTakeover"},
        )

    def test_verify_takeover_is_possession_only(self):
        op = BY_ID["verifyTakeover"]
        self.assertFalse(op.needs_bearer)
        header = [p for p in op.params if p.kind == "header"]
        self.assertEqual([p.name for p in header], ["X-Takeover-Token"])
        self.assertTrue(all(p.required for p in header))
        ts, py, go = ts_out(), py_out(), go_out()
        verify_ts = ts.split("async verifyTakeover")[1].split("\n  }")[0]
        self.assertIn(
            'async verifyTakeover(xTakeoverToken: string): Promise<TakeoverContext>', ts
        )
        self.assertIn(
            'const headers: Record<string, string> = { "X-Takeover-Token": xTakeoverToken };',
            verify_ts,
        )
        self.assertIn("bearer: false,", verify_ts)
        self.assertIn("def verify_takeover(self, x_takeover_token)", py)
        self.assertIn('headers = {"X-Takeover-Token": x_takeover_token}', py)
        self.assertIn("bearer=False,", py.split("def verify_takeover")[1].split("def ")[0])
        self.assertIn(
            "VerifyTakeover(ctx context.Context, xTakeoverToken string) (TakeoverContext, error)",
            go,
        )
        verify = go.split("func (c *Client) VerifyTakeover")[1].split("// ")[0]
        self.assertIn('headers.Set("X-Takeover-Token", xTakeoverToken)', verify)
        self.assertIn(", false)", verify)

    def test_health_is_also_anonymous(self):
        ts = ts_out().split("async health")[1].split("\n  }")[0]
        self.assertIn("bearer: false,", ts)

    def test_create_operation_requires_idempotency_key_header(self):
        op = BY_ID["createOperation"]
        self.assertIn("Idempotency-Key", [p.name for p in op.params if p.kind == "header"])
        ts = ts_out().split("async createOperation")[1].split("\n  }")[0]
        self.assertIn(
            'const headers: Record<string, string> = { "Idempotency-Key": idempotencyKey };',
            ts,
        )
        go = go_out().split("func (c *Client) CreateOperation")[1].split("// ")[0]
        self.assertIn('headers.Set("Idempotency-Key", idempotencyKey)', go)

    def test_secured_operations_do_not_disable_bearer(self):
        ts = ts_out()
        list_accounts = ts.split("async listAccounts")[1].split("\n  }")[0]
        self.assertNotIn("bearer: false", list_accounts)

    def test_stream_events_drains_text_with_last_event_id(self):
        op = BY_ID["streamEvents"]
        self.assertTrue(op.raw_response)
        ts = ts_out().split("async streamEvents")[1].split("\n  }")[0]
        self.assertIn("Promise<string>", ts)
        self.assertIn("raw: true,", ts)
        self.assertIn('headers["Last-Event-ID"] = lastEventID;', ts)
        py = py_out().split("def stream_events")[1].split("\n    def ")[0]
        self.assertIn("-> str:", py)
        self.assertIn("raw=True,", py)
        go = go_out().split("func (c *Client) StreamEvents")[1].split("// ")[0]
        self.assertIn("(string, error)", go)
        self.assertIn('headers.Set("Accept", "text/event-stream")', go)

    def test_bearer_is_injected_in_every_request_core(self):
        ts, py, go = ts_out(), py_out(), go_out()
        self.assertIn("headers.authorization = `Bearer ${key}`;", ts)
        self.assertIn('sent["authorization"] = f"Bearer {key}"', py)
        self.assertIn('"Authorization", "Bearer "+key', go)


class NamingHelpers(unittest.TestCase):
    def test_wire_names_map_to_identifiers(self):
        cases = [
            ("X-Takeover-Token", "xTakeoverToken", "x_takeover_token", "XTakeoverToken"),
            ("Idempotency-Key", "idempotencyKey", "idempotency_key", "IdempotencyKey"),
            ("Last-Event-ID", "lastEventID", "last_event_id", "LastEventID"),
            ("grant_id", "grantId", "grant_id", "GrantId"),
            ("cursor", "cursor", "cursor", "Cursor"),
            ("listAccounts", "listAccounts", "list_accounts", "ListAccounts"),
        ]
        for name, c, s, p in cases:
            self.assertEqual(gen.camel(name), c, name)
            self.assertEqual(gen.snake(name), s, name)
            self.assertEqual(gen.pascal(name), p, name)


if __name__ == "__main__":
    unittest.main()
