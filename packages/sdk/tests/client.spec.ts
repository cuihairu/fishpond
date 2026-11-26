// F036: handwritten contract tests over the GENERATED client. The
// generator proves shape; these prove behavior — bearer injection, the
// possession path, idempotency, cursor queries, SSE draining, and RFC
// 9457 classification against a mocked fetch.
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { ApiError, FishpondClient, classifyProblem } from "../src/client.js";

interface Call {
  method: string;
  url: string;
  headers: Record<string, string>;
  body?: string;
}

const calls: Call[] = [];
let responder: (call: Call) => Response;

function json(payload: unknown, status = 200): Response {
  return new Response(JSON.stringify(payload), {
    status,
    headers: { "content-type": "application/json" },
  });
}

function problem(
  slug: string,
  status: number,
  detail = "detail text",
): Response {
  return json(
    {
      type: `https://fishpond.dev/problems/${slug}`,
      title: "title text",
      status,
      detail,
    },
    status,
  );
}

function last(): Call {
  expect(calls.length).toBeGreaterThan(0);
  return calls[calls.length - 1];
}

beforeEach(() => {
  calls.length = 0;
  responder = () => json({});
  vi.stubGlobal("fetch", async (input: RequestInfo | URL, init?: RequestInit) => {
    const headers: Record<string, string> = {};
    new Headers(init?.headers).forEach((value, key) => {
      headers[key] = value;
    });
    calls.push({
      method: init?.method ?? "GET",
      url: String(input),
      headers,
      body: typeof init?.body === "string" ? init.body : undefined,
    });
    return responder(last());
  });
});

afterEach(() => {
  vi.unstubAllGlobals();
});

function client(key: string | null = "key-1"): FishpondClient {
  return new FishpondClient(
    "http://control.test",
    key === null ? undefined : () => key,
  );
}

describe("transport core", () => {
  it("strips a trailing slash from the base url", async () => {
    await new FishpondClient("http://control.test/", () => "k").health();
    expect(last().url).toBe("http://control.test/healthz");
  });

  it("injects the bearer key on secured calls", async () => {
    await client().getAccount("acc-1");
    expect(last().headers.authorization).toBe("Bearer key-1");
  });

  it("omits the bearer header when the key is empty or missing", async () => {
    await client("").listAccounts();
    await client(null).listAccounts();
    expect(calls[0].headers.authorization).toBeUndefined();
    expect(calls[1].headers.authorization).toBeUndefined();
  });

  it("sends json bodies with content type", async () => {
    await client().registerAccount({ id: "acc-9", platform: "qq" });
    expect(last().method).toBe("POST");
    expect(last().headers["content-type"]).toBe("application/json");
    expect(JSON.parse(last().body ?? "{}")).toEqual({
      id: "acc-9",
      platform: "qq",
    });
  });

  it("encodes path parameters", async () => {
    await client().getAccount("acc/sp ace");
    expect(last().url).toBe(
      "http://control.test/accounts/acc%2Fsp%20ace",
    );
  });
});

describe("anonymous operations", () => {
  it("health never sends a bearer header", async () => {
    await client().health();
    expect(last().url).toBe("http://control.test/healthz");
    expect(last().headers.authorization).toBeUndefined();
  });

  it("verifyTakeover is possession-only: token header, no bearer", async () => {
    responder = () => json({ tenant: "t-1", session: "s-1" });
    const context = await client().verifyTakeover("tok-secret");
    expect(last().method).toBe("POST");
    expect(last().url).toBe("http://control.test/takeover/verify");
    expect(last().headers["x-takeover-token"]).toBe("tok-secret");
    expect(last().headers.authorization).toBeUndefined();
    expect(context).toEqual({ tenant: "t-1", session: "s-1" });
  });
});

describe("cursor pagination", () => {
  it("sends cursor and limit as query parameters", async () => {
    await client().listSessions("c3", 5);
    expect(last().url).toBe("http://control.test/sessions?cursor=c3&limit=5");
  });

  it("omits undefined query parameters entirely", async () => {
    await client().listEvents();
    expect(last().url).toBe("http://control.test/events");
  });

  it("paginates operations and events the same way", async () => {
    await client().listOperations("c9", 1);
    await client().listEvents("e1", 200);
    expect(calls[0].url).toBe(
      "http://control.test/operations?cursor=c9&limit=1",
    );
    expect(calls[1].url).toBe(
      "http://control.test/events?cursor=e1&limit=200",
    );
  });
});

describe("idempotent operations", () => {
  it("createOperation requires the Idempotency-Key header", async () => {
    await client().createOperation("idem-7", { id: "op-1", kind: "send_message" });
    expect(last().method).toBe("POST");
    expect(last().headers["idempotency-key"]).toBe("idem-7");
    expect(JSON.parse(last().body ?? "{}")).toEqual({
      id: "op-1",
      kind: "send_message",
    });
  });

  it("claimOperation posts the attempt request", async () => {
    responder = () =>
      json({
        attempt: { attempt_id: "at-1", attempt_no: 1, deadline_at: 5 },
        operation: { id: "op-1", kind: "send_message", state: "RUNNING", created_at: 1 },
      });
    const claimed = await client().claimOperation("op-1", {
      worker_id: "w-1",
      attempt_id: "at-1",
      deadline_at: 5,
    });
    expect(last().url).toBe("http://control.test/operations/op-1/claim");
    expect(claimed.operation.state).toBe("RUNNING");
    expect(claimed.attempt.attempt_no).toBe(1);
  });

  it("closeAttempt settles an attempt", async () => {
    await client().closeAttempt("op-1", "at-1", {
      worker_id: "w-1",
      result: "succeeded",
    });
    expect(last().url).toBe(
      "http://control.test/operations/op-1/attempts/at-1/close",
    );
    expect(JSON.parse(last().body ?? "{}").result).toBe("succeeded");
  });
});

describe("session lifecycle", () => {
  it("transitions carry the action and optional credential fields", async () => {
    await client().transitionSession("s-1", {
      action: "authorize",
      credential_id: "cred-1",
      credential_version: 3,
    });
    expect(last().url).toBe("http://control.test/sessions/s-1/transition");
    const sent = JSON.parse(last().body ?? "{}");
    expect(sent.action).toBe("authorize");
    expect(sent.credential_id).toBe("cred-1");
    expect(sent.credential_version).toBe(3);
  });

  it("takeover issue/list/release use the nested grant path", async () => {
    responder = () => json({ grant_id: "g-1", token: "raw", expires_at: 9 });
    const issued = await client().startTakeover("s-1", { operator: "op", ttl_ms: 100 });
    expect(last().url).toBe("http://control.test/sessions/s-1/takeover");
    expect(issued.token).toBe("raw");
    await client().listTakeoverGrants("s-1");
    expect(last().url).toBe("http://control.test/sessions/s-1/takeover");
    expect(last().method).toBe("GET");
    await client().releaseTakeover("s-1", "g-1");
    expect(last().method).toBe("DELETE");
    expect(last().url).toBe("http://control.test/sessions/s-1/takeover/g-1");
  });
});

describe("sse drain", () => {
  it("streams text with accept and Last-Event-ID", async () => {
    responder = () => new Response("event: caught-up\ndata: {}\n", { status: 200 });
    const text = await client().streamEvents("worker-1", "e42");
    expect(last().url).toBe(
      "http://control.test/events/stream?consumer=worker-1",
    );
    expect(last().headers.accept).toBe("text/event-stream");
    expect(last().headers["last-event-id"]).toBe("e42");
    expect(text).toBe("event: caught-up\ndata: {}\n");
  });
});

describe("problem classification", () => {
  const cases: Array<[string, number, string]> = [
    ["scope-denied", 403, "forbidden"],
    ["not-found", 404, "missing"],
    ["invalid", 422, "invalid"],
    ["invalid-cursor", 400, "invalid"],
    ["conflict", 409, "conflict"],
    ["unauthorized", 401, "unauthorized"],
    ["reauth-required", 401, "reauth"],
    ["storage", 500, "server"],
    ["store-refused", 500, "server"],
    ["forbidden", 403, "forbidden"],
  ];

  for (const [slug, status, kind] of cases) {
    it(`classifies ${slug} as ${kind}`, async () => {
      responder = () => problem(slug, status);
      const error = await client()
        .getAccount("acc-1")
        .catch((caught: unknown) => caught);
      expect(error).toBeInstanceOf(ApiError);
      const apiError = error as ApiError;
      expect(apiError.kind).toBe(kind);
      expect(apiError.problem.status).toBe(status);
      expect(apiError.message).toBe("detail text");
    });
  }

  it("maps foreign problem uris to other", async () => {
    responder = () =>
      json({ type: "https://else.example/x", title: "t", status: 400 }, 400);
    const error = await client().listAccounts().catch((caught: unknown) => caught);
    expect((error as ApiError).kind).toBe("other");
  });

  it("synthesizes store-refused for non-problem bodies", async () => {
    responder = () => new Response("<html>boom</html>", { status: 502 });
    const error = await client().listAccounts().catch((caught: unknown) => caught);
    const apiError = error as ApiError;
    expect(apiError.kind).toBe("server");
    expect(apiError.problem.type).toBe(
      "https://fishpond.dev/problems/store-refused",
    );
    expect(apiError.problem.status).toBe(502);
  });

  it("classifies uris without a network call", () => {
    expect(classifyProblem("https://fishpond.dev/problems/not-found")).toBe(
      "missing",
    );
    expect(classifyProblem("https://else.example/x")).toBe("other");
  });
});

describe("typed payloads", () => {
  it("returns parsed page shapes", async () => {
    responder = () =>
      json({
        accounts: [{ id: "a", platform: "qq", state: "ONLINE", updated_at: 1 }],
      });
    const page = await client().listAccounts();
    expect(page.accounts[0]?.state).toBe("ONLINE");
  });

  it("surfaces status counts", async () => {
    responder = () => json({ total: 3, online: 1, offline: 1, unknown: 1, disabled: 0 });
    const summary = await client().accountStatus();
    expect(summary.total).toBe(3);
    expect(summary.unknown).toBe(1);
  });
});
