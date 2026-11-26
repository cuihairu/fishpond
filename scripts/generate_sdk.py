#!/usr/bin/env python3
"""F036: generate the fishpond SDKs from the served OpenAPI document.

Input is the contract-frozen fixture (crates/server/tests/fixtures/openapi.json),
which a Rust test proves byte-identical to what the server serves at
/openapi.json. Outputs are written byte-identically on every run, so
regenerating and running `git diff --exit-code` proves the committed
SDKs always match the contract:

    python3 scripts/generate_sdk.py \
        --doc crates/server/tests/fixtures/openapi.json \
        --ts packages/sdk/src/client.ts \
        --py sdk/python/fishpond/__init__.py \
        --go sdk/go/client.go --gomod sdk/go/go.mod

Standard library only. All three clients share the same semantics:
bearer injection for secured operations, RFC 9457 problem documents
raised as classified errors, the X-Takeover-Token possession path,
and text bodies for the SSE drain operation.
"""

from __future__ import annotations

import argparse
import json
import re
import shutil
import subprocess
from dataclasses import dataclass, field
from pathlib import Path

HTTP_METHODS = ("get", "post", "put", "patch", "delete")

PROBLEM_BASE = "https://fishpond.dev/problems/"

PROBLEM_KINDS: dict[str, str] = {
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

GENERATED_NOTE = (
    "GENERATED FILE — scripts/generate_sdk.py from "
    "crates/server/tests/fixtures/openapi.json. Do not edit by hand."
)


@dataclass
class Param:
    name: str  # wire name, possibly dashed ("X-Takeover-Token")
    required: bool
    kind: str  # "path" | "header" | "query"
    schema_type: str = "string"  # "string" | "integer"


@dataclass
class Operation:
    op_id: str
    method: str  # upper case
    path: str  # template with {placeholders}
    summary: str
    params: list[Param] = field(default_factory=list)
    body_ref: str | None = None
    return_ref: str | None = None  # named schema of the 2xx JSON body
    raw_response: bool = False  # SSE/text body instead of JSON
    needs_bearer: bool = True

    def of_kind(self, kind: str) -> list[Param]:
        return [p for p in self.params if p.kind == kind]


def ref_name(schema: dict) -> str | None:
    ref = schema.get("$ref")
    return ref.rsplit("/", 1)[-1] if isinstance(ref, str) else None


def analyze(doc: dict) -> list[Operation]:
    ops: list[Operation] = []
    for path in sorted(doc.get("paths", {})):
        for method in HTTP_METHODS:
            item = doc["paths"][path].get(method)
            if item is None or "operationId" not in item:
                continue
            placeholders = {p.strip("{}") for p in path.split("/") if p.startswith("{")}
            op = Operation(
                op_id=item["operationId"],
                method=method.upper(),
                path=path,
                summary=item.get("summary", ""),
            )
            for declared in item.get("parameters", []):
                schema = declared.get("schema", {})
                kind = (
                    "path"
                    if declared.get("in") == "path"
                    else "query"
                    if declared.get("in") == "query"
                    else "header"
                )
                op.params.append(
                    Param(
                        name=declared["name"],
                        required=bool(declared.get("required")),
                        kind=kind,
                        schema_type="integer" if schema.get("type") == "integer" else "string",
                    )
                )
            assert placeholders <= {p.name for p in op.params}, (
                f"{op.op_id}: path placeholders not fully documented"
            )
            body = (
                item.get("requestBody", {})
                .get("content", {})
                .get("application/json", {})
                .get("schema")
            )
            op.body_ref = ref_name(body) if body else None
            for status in sorted(item.get("responses", {})):
                if status.startswith("2"):
                    content = item["responses"][status].get("content", {})
                    if "application/json" in content:
                        op.return_ref = ref_name(content["application/json"]["schema"])
                    elif "text/event-stream" in content:
                        op.raw_response = True
                    break
            # The document has no top-level security default: an
            # operation is secured iff it explicitly requires
            # bearerAuth. Absent or empty `security` means anonymous
            # (health probe, the takeover possession path).
            security = item.get("security", [])
            op.needs_bearer = any("bearerAuth" in entry for entry in security)
            ops.append(op)
    ops.sort(key=lambda op: op.op_id)
    return ops


# --- naming -----------------------------------------------------------


def words(name: str) -> list[str]:
    return [part for part in re.split(r"[-_]", name) if part]


def camel(name: str) -> str:
    """lowerCamel identifier; input may be camelCase or dashed wire name."""
    head, *rest = words(name)
    return head[:1].lower() + head[1:] + "".join(p[:1].upper() + p[1:] for p in rest)


def snake(name: str) -> str:
    """snake_case identifier; input may be camelCase or dashed wire name."""
    out: list[str] = []
    text = name.replace("-", "_")
    for index, ch in enumerate(text):
        if ch.isupper() and index > 0:
            prev = text[index - 1]
            nxt = text[index + 1] if index + 1 < len(text) else ""
            if prev != "_" and (not prev.isupper() or (nxt and nxt.islower())):
                out.append("_")
        out.append(ch.lower())
    return "".join(out)


def pascal(name: str) -> str:
    """UpperCamel identifier; inner case preserved ("listAccounts" -> "ListAccounts")."""
    return "".join(part[:1].upper() + part[1:] for part in words(name))


def classify_entries(language: str) -> list[str]:
    lines = []
    for slug in sorted(PROBLEM_KINDS):
        kind = PROBLEM_KINDS[slug]
        if language == "ts":
            lines.append(f'  "{slug}": "{kind}",')
        elif language == "py":
            lines.append(f'    "{slug}": "{kind}",')
        else:
            lines.append(f'\tcase "{slug}":')
            lines.append(f'\t\treturn "{kind}"')
    return lines


def classify_problem(type_uri: str) -> str:
    """Reference implementation of the classification every SDK embeds."""
    if not type_uri.startswith(PROBLEM_BASE):
        return "other"
    return PROBLEM_KINDS.get(type_uri.rsplit("/", 1)[-1], "other")


# --- TypeScript emitter -------------------------------------------------


def ts_type(schema: dict) -> str:
    name = ref_name(schema)
    if name is not None:
        return name
    if "enum" in schema:
        return " | ".join(json.dumps(value) for value in schema["enum"])
    kind = schema.get("type")
    if isinstance(kind, list):
        return " | ".join(ts_type({**schema, "type": part}) for part in kind)
    if kind == "null":
        return "null"
    if kind == "string":
        return "string"
    if kind in ("integer", "number"):
        return "number"
    if kind == "boolean":
        return "boolean"
    if kind == "array":
        return f"{ts_type(schema.get('items', {}))}[]"
    if kind == "object":
        props = schema.get("properties")
        if not props:
            return "Record<string, unknown>"
        required = set(schema.get("required", []))
        fields = [
            f"{key}{'?' if key not in required else ''}: {ts_type(props[key])}"
            for key in props
        ]
        return "{ " + "; ".join(fields) + " }"
    return "unknown"


def emit_ts(doc: dict, ops: list[Operation]) -> str:
    out: list[str] = [f"// {GENERATED_NOTE}", ""]
    out += [
        "/** One RFC 9457 problem document. */",
        "export interface Problem {",
        "  type: string;",
        "  title: string;",
        "  status: number;",
        "  detail?: string;",
        "}",
        "",
    ]
    for name in sorted(doc.get("components", {}).get("schemas", {})):
        if name == "Problem":
            continue  # emitted by hand above; identical shape
        schema = doc["components"]["schemas"][name]
        required = set(schema.get("required", []))
        out.append(f"export interface {name} {{")
        for key, prop in schema.get("properties", {}).items():
            if prop.get("description"):
                out.append(f"  /** {prop['description']} */")
            opt = "" if key in required else "?"
            out.append(f"  {key}{opt}: {ts_type(prop)};")
        out.append("}")
    kinds = sorted(set(PROBLEM_KINDS.values()) | {"other"})
    out += ["", "export type ProblemKind ="]
    out += [f'  | "{kind}"' for kind in kinds]
    out += [
        ";",
        "",
        "const PROBLEM_KINDS: Record<string, ProblemKind> = {",
    ]
    out += classify_entries("ts")
    out += [
        "};",
        "",
        "/** Maps a problem type URI to the SDK's failure classes. */",
        "export function classifyProblem(typeUri: string): ProblemKind {",
        f'  if (!typeUri.startsWith("{PROBLEM_BASE}")) return "other";',
        '  return PROBLEM_KINDS[typeUri.split("/").pop() ?? ""] ?? "other";',
        "}",
        "",
        "/** One failed call: the parsed problem plus its class. */",
        "export class ApiError extends Error {",
        "  readonly problem: Problem;",
        "  readonly kind: ProblemKind;",
        "",
        "  constructor(problem: Problem, kind: ProblemKind) {",
        "    super(problem.detail ?? problem.title);",
        '    this.name = "ApiError";',
        "    this.problem = problem;",
        "    this.kind = kind;",
        "  }",
        "}",
        "",
        "interface RequestOptions {",
        "  bearer?: boolean;",
        "  headers?: Record<string, string>;",
        "  body?: unknown;",
        "  raw?: boolean;",
        "}",
        "",
        "/** Typed client for the fishpond control plane. */",
        "export class FishpondClient {",
        "  private readonly baseUrl: string;",
        "  private readonly key: (() => string | undefined) | undefined;",
        "",
        "  constructor(baseUrl: string, key?: () => string | undefined) {",
        '    this.baseUrl = baseUrl.replace(/\\/+$/, "");',
        "    this.key = key;",
        "  }",
        "",
        "  private async request<T>(",
        "    method: string,",
        "    path: string,",
        "    options: RequestOptions = {},",
        "  ): Promise<T> {",
        "    const headers: Record<string, string> = { ...(options.headers ?? {}) };",
        "    const key = this.key?.();",
        '    if (options.bearer !== false && key !== undefined && key !== "") {',
        "      headers.authorization = `Bearer ${key}`;",
        "    }",
        '    if (options.raw) headers.accept = "text/event-stream";',
        "    let bodyText: string | undefined;",
        "    if (options.body !== undefined) {",
        '      headers["content-type"] = "application/json";',
        "      bodyText = JSON.stringify(options.body);",
        "    }",
        "    const response = await fetch(`${this.baseUrl}${path}`, {",
        "      method,",
        "      headers,",
        "      body: bodyText,",
        "    });",
        "    if (!response.ok) {",
        "      const text = await response.text();",
        "      let problem: Problem;",
        "      try {",
        "        problem = JSON.parse(text) as Problem;",
        "      } catch {",
        "        problem = {",
        '          type: "' + PROBLEM_BASE + 'store-refused",',
        '          title: "error",',
        "          status: response.status,",
        "          detail: `control plane answered ${response.status} with a non-problem body`,",
        "        };",
        "      }",
        "      throw new ApiError(problem, classifyProblem(problem.type));",
        "    }",
        "    if (options.raw) return (await response.text()) as unknown as T;",
        "    if (response.status === 204) return undefined as T;",
        "    return (await response.json()) as T;",
        "  }",
    ]
    for op in ops:
        out += ["", f"  /** {op.summary} */"]
        headers = op.of_kind("header")
        queries = op.of_kind("query")
        args = [
            f"{camel(p.name)}: string" for p in op.of_kind("path") + op.of_kind("header") if p.required
        ]
        if op.body_ref:
            args.append(f"body: {op.body_ref}")
        args += [
            f"{camel(p.name)}?: {'number' if p.schema_type == 'integer' else 'string'}"
            for p in queries + [p for p in headers if not p.required]
        ]
        returns = "string" if op.raw_response else (op.return_ref or "void")
        out.append(f"  async {op.op_id}({', '.join(args)}): Promise<{returns}> {{")
        path_expr = op.path
        for param in op.of_kind("path"):
            path_expr = path_expr.replace(
                "{" + param.name + "}",
                "${encodeURIComponent(" + camel(param.name) + ")}",
            )
        out.append(f"    const path = `{path_expr}`;")
        if headers:
            required = {p.name: camel(p.name) for p in headers if p.required}
            entries = ", ".join(f'"{k}": {v}' for k, v in required.items())
            if entries:
                out.append(f"    const headers: Record<string, string> = {{ {entries} }};")
            else:
                out.append("    const headers: Record<string, string> = {};")
            for param in [p for p in headers if not p.required]:
                local = camel(param.name)
                out.append(
                    f'    if ({local} !== undefined) headers["{param.name}"] = {local};'
                )
        if queries:
            out.append("    const query = new URLSearchParams();")
            for param in queries:
                local = camel(param.name)
                out.append(
                    f'    if ({local} !== undefined) query.set("{param.name}", String({local}));'
                )
            out.append("    const qs = query.toString();")
            out.append(
                '    return this.request<'
                + returns
                + '>("'
                + op.method
                + '", qs === "" ? path : `${path}?${qs}`, {'
            )
        else:
            out.append(f'    return this.request<{returns}>("{op.method}", path, {{')
        options: list[str] = []
        if not op.needs_bearer:
            options.append("bearer: false")
        if headers:
            options.append("headers")
        if op.body_ref:
            options.append("body")
        if op.raw_response:
            options.append("raw: true")
        for entry in options:
            out.append(f"      {entry},")
        out += ["    });", "  }"]
    out += ["}", ""]
    return "\n".join(out) + "\n"


# --- Python emitter ------------------------------------------------------


def emit_py(doc: dict, ops: list[Operation]) -> str:
    out: list[str] = [
        '"""fishpond control-plane SDK.',
        "",
        GENERATED_NOTE,
        '"""',
        "from __future__ import annotations",
        "",
        "import json",
        "import urllib.error",
        "import urllib.request",
        "from urllib.parse import quote, urlencode",
        "",
        f'PROBLEM_BASE = "{PROBLEM_BASE}"',
        "",
        "PROBLEM_KINDS = {",
    ]
    out += classify_entries("py")
    out += [
        "}",
        "",
        "",
        "def classify_problem(type_uri: str) -> str:",
        """    \"\"\"Maps a problem type URI to the SDK's failure classes.\"\"\"""",
        "    if not type_uri.startswith(PROBLEM_BASE):",
        '        return "other"',
        '    return PROBLEM_KINDS.get(type_uri.rsplit("/", 1)[-1], "other")',
        "",
        "",
        "class FishpondError(Exception):",
        '    """One failed call: the problem document plus its class."""',
        "",
        "    def __init__(self, problem: dict, kind: str, status: int) -> None:",
        "        self.problem = problem",
        "        self.kind = kind",
        "        self.status = status",
        '        detail = problem.get("detail") or problem.get("title")',
        '        super().__init__(f"{kind}: {detail}")',
        "",
        "",
        "class FishpondClient:",
        "    \"\"\"Typed client for the fishpond control plane.",
        "",
        "    `transport` is a test seam: a callable",
        "    (method, url, headers, body) -> (status, payload_bytes).",
        "    \"\"\"",
        "",
        "    def __init__(self, base_url: str, key=None, transport=None) -> None:",
        '        self.base_url = base_url.rstrip("/")',
        "        self._key = key",
        "        self._transport = transport",
        "",
        "    def _request(",
        "        self,",
        "        method: str,",
        "        path: str,",
        "        *,",
        "        path_args=None,",
        "        headers=None,",
        "        body=None,",
        "        bearer=True,",
        "        raw=False,",
        "        query=None,",
        "    ):",
        "        for name, value in (path_args or {}).items():",
        '            path = path.replace("{" + name + "}", quote(str(value), safe=""))',
        "        clean = {k: v for k, v in (query or {}).items() if v is not None}",
        "        if clean:",
        '            path = path + "?" + urlencode(clean)',
        "        sent = dict(headers or {})",
        "        key = self._key() if callable(self._key) else self._key",
        "        if bearer and key:",
        '            sent["authorization"] = f"Bearer {key}"',
        "        data = None",
        "        if body is not None:",
        '            sent["content-type"] = "application/json"',
        "            data = json.dumps(body).encode()",
        "        if raw:",
        '            sent["accept"] = "text/event-stream"',
        "        url = self.base_url + path",
        "        if self._transport is not None:",
        "            status, payload = self._transport(method, url, sent, data)",
        "        else:",
        "            request = urllib.request.Request(url, data=data, method=method, headers=sent)",
        "            try:",
        "                with urllib.request.urlopen(request) as response:",
        "                    status = response.status",
        "                    payload = response.read()",
        "            except urllib.error.HTTPError as err:",
        "                status = err.code",
        "                payload = err.read()",
        "        if status >= 300:",
        "            try:",
        "                problem = json.loads(payload)",
        "            except Exception:",
        "                problem = {",
        '                    "type": PROBLEM_BASE + "store-refused",',
        '                    "title": "error",',
        '                    "status": status,',
        '                    "detail": f"control plane answered {status} with a non-problem body",',
        "                }",
        '            raise FishpondError(problem, classify_problem(problem.get("type", "")), status)',
        "        if raw:",
        "            return payload.decode()",
        "        if not payload:",
        "            return None",
        "        return json.loads(payload)",
    ]
    for op in ops:
        out += ["", ""]
        headers = op.of_kind("header")
        queries = op.of_kind("query")
        args = [snake(p.name) for p in op.of_kind("path") + headers if p.required]
        if op.body_ref:
            args.append("body: dict")
        args += [
            f"{snake(p.name)}: {'int' if p.schema_type == 'integer' else 'str'} | None = None"
            for p in queries + [p for p in headers if not p.required]
        ]
        returns = "str" if op.raw_response else (op.return_ref or "None")
        out.append(f"    def {snake(op.op_id)}(self, {', '.join(args)}) -> {returns}:")
        docline = op.summary
        if op.body_ref:
            docline += f"\n\n        Request body schema: {op.body_ref}."
        out.append(f'        """{docline}"""')
        if headers:
            required = {p.name: snake(p.name) for p in headers if p.required}
            entries = ", ".join(f'"{k}": {v}' for k, v in required.items())
            out.append(f"        headers = {{{entries}}}")
            for param in [p for p in headers if not p.required]:
                local = snake(param.name)
                out.append("        if " + local + " is not None:")
                out.append(f'            headers["{param.name}"] = {local}')
        out.append("        return self._request(")
        out.append(f'            "{op.method}",')
        out.append(f'            "{op.path}",')
        if op.of_kind("path"):
            mapping = ", ".join(f'"{p.name}": {snake(p.name)}' for p in op.of_kind("path"))
            out.append(f"            path_args={{{mapping}}},")
        if headers:
            out.append("            headers=headers,")
        if not op.needs_bearer:
            out.append("            bearer=False,")
        if op.body_ref:
            out.append("            body=body,")
        if op.raw_response:
            out.append("            raw=True,")
        if queries:
            mapping = ", ".join(f'"{p.name}": {snake(p.name)}' for p in queries)
            out.append(f"            query={{{mapping}}},")
        out.append("        )")
    out += ["", ""]
    return "\n".join(out) + "\n"


# --- Go emitter -----------------------------------------------------------


def go_type(parent: str, key: str, schema: dict, optional: bool, nested: dict) -> str:
    name = ref_name(schema)
    if name is not None:
        return ("*" + name) if optional else name
    if "enum" in schema:
        return ("*string") if optional else "string"
    kind = schema.get("type")
    if isinstance(kind, list):
        remaining = [k for k in kind if k != "null"]
        if len(remaining) != 1:
            return "any"  # unions beyond nullable have no Go equivalent
        base = go_type(parent, key, {**schema, "type": remaining[0]}, False, nested)
        return ("*" + base) if optional else base
    if kind == "string":
        return ("*string") if optional else "string"
    if kind in ("integer", "number"):
        return ("*int64") if optional else "int64"
    if kind == "boolean":
        return ("*bool") if optional else "bool"
    if kind == "array":
        return "[]" + go_type(parent, key, schema.get("items", {}), False, nested)
    if kind == "object":
        props = schema.get("properties")
        if not props:
            return "map[string]any"
        struct_name = parent + pascal(key)
        if struct_name not in nested:
            nested[struct_name] = go_struct_decl(struct_name, schema, nested)
        return ("*" + struct_name) if optional else struct_name
    return "any"


def go_struct_decl(name: str, schema: dict, nested: dict) -> list[str]:
    """One struct declaration; inline object properties become named nested structs."""
    out = ["", f"// {name} mirrors the contract schema of the same name.", f"type {name} struct {{"]
    required = set(schema.get("required", []))
    for key, prop in schema.get("properties", {}).items():
        optional = key not in required
        tag = key + (",omitempty" if optional else "")
        field_type = go_type(name, key, prop, optional, nested)
        out.append(f"\t{pascal(key)} {field_type} `json:\"{tag}\"`")
    out.append("}")
    return out


def gofmt(text: str) -> str:
    """Canonical Go formatting; falls back to raw text without gofmt."""
    binary = shutil.which("gofmt")
    if binary is None:
        return text
    return subprocess.run(
        [binary], input=text, capture_output=True, text=True, check=True
    ).stdout


def emit_go(doc: dict, ops: list[Operation]) -> str:
    needs_strconv = any(
        p.schema_type == "integer" for op in ops for p in op.of_kind("query")
    )
    needs_query = any(op.of_kind("query") for op in ops)
    needs_body = any(op.body_ref for op in ops)
    out: list[str] = [f"// {GENERATED_NOTE}", "package fishpond", "", "import ("]
    if needs_body:
        out.append('\t"bytes"')
    out += [
        '\t"context"',
        '\t"encoding/json"',
        '\t"fmt"',
        '\t"io"',
        '\t"net/http"',
    ]
    if needs_query:
        out.append('\t"net/url"')
    if needs_strconv:
        out.append('\t"strconv"')
    out.append('\t"strings"')
    out += [
        ")",
        "",
        f'const problemBase = "{PROBLEM_BASE}"',
        "",
        "// Problem is one RFC 9457 problem document.",
        "type Problem struct {",
        '\tType   string `json:"type"`',
        '\tTitle  string `json:"title"`',
        '\tStatus int    `json:"status"`',
        '\tDetail string `json:"detail"`',
        "}",
        "",
        "// Error is one failed call: the parsed problem plus its class.",
        "type Error struct {",
        "\tKind    string",
        "\tStatus  int",
        "\tProblem Problem",
        "}",
        "",
        "func (e *Error) Error() string {",
        "\tdetail := e.Problem.Detail",
        '\tif detail == "" {',
        "\t\tdetail = e.Problem.Title",
        "\t}",
        '\treturn fmt.Sprintf("%s: %s", e.Kind, detail)',
        "}",
        "",
        "// ClassifyProblem maps a problem type URI to the SDK's failure classes.",
        "func ClassifyProblem(typeURI string) string {",
        "\tif !strings.HasPrefix(typeURI, problemBase) {",
        '\t\treturn "other"',
        "\t}",
        "\ttail := typeURI",
        '\tif idx := strings.LastIndex(typeURI, "/"); idx >= 0 {',
        "\t\ttail = typeURI[idx+1:]",
        "\t}",
        "\tswitch tail {",
    ]
    out += classify_entries("go")
    out += [
        "\tdefault:",
        '\t\treturn "other"',
        "\t}",
        "}",
    ]
    nested: dict[str, list[str]] = {}
    for name in sorted(doc.get("components", {}).get("schemas", {})):
        if name == "Problem":
            continue  # emitted by hand above; identical shape
        out += go_struct_decl(name, doc["components"]["schemas"][name], nested)
    for name in sorted(nested):
        out += nested[name]
    out += [
        "",
        "// Client is a typed client for the fishpond control plane.",
        "type Client struct {",
        "\tBaseURL string",
        "\tHTTP    *http.Client",
        "\tKey     func() string",
        "}",
        "",
        "// NewClient builds a client; key may be nil for anonymous use.",
        "func NewClient(baseURL string, key func() string) *Client {",
        "\treturn &Client{BaseURL: baseURL, HTTP: http.DefaultClient, Key: key}",
        "}",
        "",
        "// do performs one call and returns the raw 2xx body.",
        "func (c *Client) do(ctx context.Context, method, path string, query url.Values, headers http.Header, body any, bearer bool) ([]byte, error) {",
        "\tif query != nil {",
        '\t\tif encoded := query.Encode(); encoded != "" {',
        '\t\t\tpath += "?" + encoded',
        "\t\t}",
        "\t}",
        "\tvar reader io.Reader",
        "\tif body != nil {",
        "\t\tencoded, err := json.Marshal(body)",
        "\t\tif err != nil {",
        "\t\t\treturn nil, err",
        "\t\t}",
        "\t\treader = bytes.NewReader(encoded)",
        "\t}",
        "\trequest, err := http.NewRequestWithContext(ctx, method, c.BaseURL+path, reader)",
        "\tif err != nil {",
        "\t\treturn nil, err",
        "\t}",
        "\tif body != nil {",
        '\t\trequest.Header.Set("Content-Type", "application/json")',
        "\t}",
        "\tfor key, values := range headers {",
        "\t\tfor _, value := range values {",
        "\t\t\trequest.Header.Add(key, value)",
        "\t\t}",
        "\t}",
        "\tif bearer && c.Key != nil {",
        '\t\tif key := c.Key(); key != "" {',
        '\t\t\trequest.Header.Set("Authorization", "Bearer "+key)',
        "\t\t}",
        "\t}",
        "\tresponse, err := c.HTTP.Do(request)",
        "\tif err != nil {",
        "\t\treturn nil, err",
        "\t}",
        "\tdefer response.Body.Close()",
        "\tif response.StatusCode >= 300 {",
        "\t\tvar problem Problem",
        '\t\tif err := json.NewDecoder(response.Body).Decode(&problem); err != nil || problem.Type == "" {',
        "\t\t\tproblem = Problem{",
        '\t\t\t\tType:   problemBase + "store-refused",',
        '\t\t\t\tTitle:  "error",',
        "\t\t\t\tStatus: response.StatusCode,",
        '\t\t\t\tDetail: fmt.Sprintf("control plane answered %d with a non-problem body", response.StatusCode),',
        "\t\t\t}",
        "\t\t}",
        "\t\treturn nil, &Error{Kind: ClassifyProblem(problem.Type), Status: response.StatusCode, Problem: problem}",
        "\t}",
        "\treturn io.ReadAll(response.Body)",
        "}",
    ]
    for op in ops:
        out += ["", f"// {op.summary}"]
        headers = op.of_kind("header")
        queries = op.of_kind("query")
        args = ["ctx context.Context"]
        args += [f"{camel(p.name)} string" for p in op.of_kind("path") + headers if p.required]
        if op.body_ref:
            args.append(f"body {op.body_ref}")
        args += [
            f"{camel(p.name)} *{'int64' if p.schema_type == 'integer' else 'string'}"
            for p in queries + [p for p in headers if not p.required]
        ]
        if op.raw_response:
            returns = "string, error"
        elif op.return_ref:
            returns = f"{op.return_ref}, error"
        else:
            returns = "error"
        out.append(
            f"func (c *Client) {pascal(op.op_id)}({', '.join(args)}) ({returns}) {{"
        )
        if queries:
            out.append("\tquery := url.Values{}")
            for param in queries:
                local = camel(param.name)
                if param.schema_type == "integer":
                    out.append(
                        f"\tif {local} != nil {{ query.Set(\"{param.name}\", strconv.FormatInt(*{local}, 10)) }}"
                    )
                else:
                    out.append(f'\tif {local} != nil {{ query.Set("{param.name}", *{local}) }}')
        if headers or op.raw_response:
            out.append("\theaders := http.Header{}")
            for param in headers:
                if param.required:
                    out.append(f'\theaders.Set("{param.name}", {camel(param.name)})')
                else:
                    local = camel(param.name)
                    out.append(f"\tif {local} != nil {{")
                    out.append(f'\t\theaders.Set("{param.name}", *{local})')
                    out.append("\t}")
            if op.raw_response:
                out.append('\theaders.Set("Accept", "text/event-stream")')
        query_arg = "query" if queries else "nil"
        headers_arg = "headers" if (headers or op.raw_response) else "nil"
        body_arg = "body" if op.body_ref else "nil"
        bearer_arg = "true" if op.needs_bearer else "false"
        path_expr = op.path
        for param in op.of_kind("path"):
            path_expr = path_expr.replace(
                "{" + param.name + "}",
                "\" + url.PathEscape(" + camel(param.name) + ") + \"",
            )
        call = (
            f'c.do(ctx, "{op.method}", "{path_expr}", {query_arg}, {headers_arg}, {body_arg}, {bearer_arg})'
        )
        if op.raw_response:
            out.append("\tpayload, err := " + call)
            out.append("\tif err != nil {")
            out.append('\t\treturn "", err')
            out.append("\t}")
            out.append("\treturn string(payload), nil")
        elif op.return_ref:
            out.append(f"\tvar out {op.return_ref}")
            out.append("\tpayload, err := " + call)
            out.append("\tif err != nil {")
            out.append("\t\treturn out, err")
            out.append("\t}")
            out.append("\tif err := json.Unmarshal(payload, &out); err != nil {")
            out.append("\t\treturn out, err")
            out.append("\t}")
            out.append("\treturn out, nil")
        else:
            out.append("\t_, err := " + call)
            out.append("\treturn err")
        out.append("}")
    out += ["", ""]
    return gofmt("\n".join(out) + "\n")


GOMOD = "module github.com/cuihairu/fishpond/sdk/go\n\ngo 1.25\n"


def main() -> None:
    parser = argparse.ArgumentParser(description="Generate fishpond SDKs from OpenAPI.")
    parser.add_argument("--doc", required=True)
    parser.add_argument("--ts", required=True)
    parser.add_argument("--py", required=True)
    parser.add_argument("--go", required=True)
    parser.add_argument("--gomod", required=True)
    args = parser.parse_args()
    doc = json.loads(Path(args.doc).read_text())
    ops = analyze(doc)
    writes = (
        (emit_ts(doc, ops), args.ts),
        (emit_py(doc, ops), args.py),
        (emit_go(doc, ops), args.go),
        (GOMOD, args.gomod),
    )
    for text, target in writes:
        path = Path(target)
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text)
    print(f"wrote {len(ops)} operations across {len(writes)} files")


if __name__ == "__main__":
    main()
