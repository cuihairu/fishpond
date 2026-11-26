// F035: the console API client — request building, bearer injection,
// problem classification, and the takeover calls. The client must
// treat every non-2xx as a classified problem document, never as a
// string, so pages can render permissions and states correctly.

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { ApiError, ConsoleApi, classifyProblem } from "../src/api.js";

function jsonResponse(status: number, body: unknown): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: { "content-type": "application/json" },
  });
}

function problemResponse(status: number, type: string, detail?: string): Response {
  return new Response(
    JSON.stringify({
      type: `https://fishpond.dev/problems/${type}`,
      title: type,
      status,
      ...(detail ? { detail } : {}),
    }),
    {
      status,
      headers: { "content-type": "application/problem+json" },
    },
  );
}

describe("classifyProblem", () => {
  it("maps the known problem classes", () => {
    expect(classifyProblem("https://fishpond.dev/problems/forbidden")).toBe("forbidden");
    expect(classifyProblem("https://fishpond.dev/problems/not-found")).toBe("missing");
    expect(classifyProblem("https://fishpond.dev/problems/invalid")).toBe("invalid");
    expect(classifyProblem("https://fishpond.dev/problems/conflict")).toBe("conflict");
    expect(classifyProblem("https://fishpond.dev/problems/unauthorized")).toBe("unauthorized");
    expect(classifyProblem("https://fishpond.dev/problems/reauth-required")).toBe("reauth");
    expect(classifyProblem("https://fishpond.dev/problems/store-refused")).toBe("server");
  });

  it("leaves unknown classes as other", () => {
    expect(classifyProblem("https://fishpond.dev/problems/something-new")).toBe("other");
    expect(classifyProblem("https://elsewhere.example/problems/forbidden")).toBe("other");
  });

  it("maps the slugs the server actually emits", () => {
    expect(classifyProblem("https://fishpond.dev/problems/scope-denied")).toBe("forbidden");
    expect(classifyProblem("https://fishpond.dev/problems/storage")).toBe("server");
    expect(classifyProblem("https://fishpond.dev/problems/invalid-cursor")).toBe("invalid");
  });
});

describe("ConsoleApi", () => {
  const fetchMock = vi.fn();
  const api = new ConsoleApi("http://control.test", () => "key-secret");

  beforeEach(() => {
    vi.stubGlobal("fetch", fetchMock);
    fetchMock.mockReset();
  });

  afterEach(() => {
    vi.unstubAllGlobals();
  });

  it("sends the bearer key and the JSON content type", async () => {
    fetchMock.mockResolvedValue(jsonResponse(200, { accounts: [] }));
    await api.listAccounts();
    expect(fetchMock).toHaveBeenCalledOnce();
    const [url, init] = fetchMock.mock.calls[0] as [string, RequestInit];
    expect(url).toBe("http://control.test/accounts");
    expect((init.headers as Record<string, string>).authorization).toBe("Bearer key-secret");
    expect(init.method).toBe("GET");
  });

  it("omits the bearer header when no key is configured", async () => {
    const anonymous = new ConsoleApi("http://control.test", () => undefined);
    fetchMock.mockResolvedValue(jsonResponse(200, { status: "ok" }));
    await anonymous.health();
    const [, init] = fetchMock.mock.calls[0] as [string, RequestInit];
    expect((init.headers as Record<string, string>).authorization).toBeUndefined();
  });

  it("raises a classified ApiError with the parsed problem document", async () => {
    fetchMock.mockResolvedValue(problemResponse(403, "forbidden", "Key lacks the required scope"));
    const failure = await api.listAccounts().catch((error: unknown) => error);
    expect(failure).toBeInstanceOf(ApiError);
    const problem = (failure as ApiError).problem;
    expect(problem.status).toBe(403);
    expect(problem.detail).toContain("scope");
    expect((failure as ApiError).kind).toBe("forbidden");
  });

  it("classifies a non-problem error body as a server problem", async () => {
    fetchMock.mockResolvedValue(new Response("<html>boom</html>", { status: 500 }));
    const failure = await api.listAccounts().catch((error: unknown) => error);
    expect((failure as ApiError).kind).toBe("server");
    expect((failure as ApiError).problem.detail).toContain("500");
  });

  it("posts JSON bodies (register account)", async () => {
    fetchMock.mockResolvedValue(jsonResponse(201, { id: "acc-1" }));
    await api.registerAccount({ id: "acc-1", platform: "mock" });
    const [, init] = fetchMock.mock.calls[0] as [string, RequestInit];
    expect(init.method).toBe("POST");
    expect(init.body).toBe(JSON.stringify({ id: "acc-1", platform: "mock" }));
    expect((init.headers as Record<string, string>)["content-type"]).toBe("application/json");
  });

  it("issues a takeover and carries the returned token once", async () => {
    fetchMock.mockResolvedValue(
      jsonResponse(201, { grant_id: "tg-1", token: "fp_tk_abc", expires_at: 1_000 }),
    );
    const issued = await api.issueTakeover("sess-1", { operator: "op", ttl_ms: 60_000 });
    expect(issued.token).toBe("fp_tk_abc");
    const [url, init] = fetchMock.mock.calls[0] as [string, RequestInit];
    expect(url).toBe("http://control.test/sessions/sess-1/takeover");
    expect(init.method).toBe("POST");
  });

  it("verifies a takeover by header alone, with no bearer key", async () => {
    const anonymous = new ConsoleApi("http://control.test", () => undefined);
    fetchMock.mockResolvedValue(
      jsonResponse(200, { grant_id: "tg-1", tenant: "t", account: "a", session: "s", operator: "op", expires_at: 1_000 }),
    );
    await anonymous.verifyTakeover("fp_tk_abc");
    const [url, init] = fetchMock.mock.calls[0] as [string, RequestInit];
    expect(url).toBe("http://control.test/takeover/verify");
    expect(init.method).toBe("POST");
    expect((init.headers as Record<string, string>)["x-takeover-token"]).toBe("fp_tk_abc");
    expect((init.headers as Record<string, string>).authorization).toBeUndefined();
  });

  it("releases a takeover with DELETE", async () => {
    fetchMock.mockResolvedValue(jsonResponse(200, { released: true }));
    await api.releaseTakeover("sess-1", "tg-1");
    const [url, init] = fetchMock.mock.calls[0] as [string, RequestInit];
    expect(url).toBe("http://control.test/sessions/sess-1/takeover/tg-1");
    expect(init.method).toBe("DELETE");
  });
});
