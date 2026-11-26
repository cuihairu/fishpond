import { createServer, type Server } from "node:http";
import { afterEach, beforeEach, describe, expect, it } from "vitest";
import { BrowserWorker, RemotePageProvider, browserAvailable } from "./index";
import { ResourceQueue } from "./resource-queue";

// The serve()-into-a-real-page integration runs where a chromium
// binary is installed; the fetch face runs everywhere (it is plain
// HTTP against a loopback fixture server — no browser involved).
const browserSuite = (await browserAvailable()) ? describe : describe.skip;

const FIXTURES = new Map<string, string>([
  ["login", "<html><head><title>remote login</title></head><body>ok</body></html>"],
]);

let server: Server;
let origin = "";

beforeEach(async () => {
  server = createServer((req, res) => {
    const name = decodeURIComponent((req.url ?? "").replace(/^\//, ""));
    const html = FIXTURES.get(name);
    if (html === undefined) {
      res.writeHead(404).end("no such fixture");
      return;
    }
    res.writeHead(200, { "content-type": "text/html" }).end(html);
  });
  await new Promise<void>((resolve) => server.listen(0, "127.0.0.1", resolve));
  origin = `http://127.0.0.1:${(server.address() as { port: number }).port}`;
});

afterEach(async () => {
  // Resolves on error too: the closed-origin test already stopped
  // the server, and closing twice reports ERR_SERVER_NOT_RUNNING.
  await new Promise<void>((resolve) => server.close(() => resolve()));
});

describe("remote page provider fetch face", () => {
  it("fetches controlled content by target", async () => {
    const provider = new RemotePageProvider({ baseUrl: origin });
    expect(provider.name).toBe("remote");
    expect(provider.urlFor("login")).toBe(`${origin}/login`);
    expect(await provider.fetchContent("login")).toContain("remote login");
  });

  it("resolves targets to one path segment under the configured origin", () => {
    const provider = new RemotePageProvider({ baseUrl: `${origin}/sub` });
    // A target cannot smuggle a path (encoded) or a host (it is a
    // fragment of the URL's path, not its authority).
    expect(provider.urlFor("deep/target")).toBe(`${origin}/sub/deep%2Ftarget`);
    expect(provider.urlFor("https://evil.example/x")).toBe(
      `${origin}/sub/https%3A%2F%2Fevil.example%2Fx`,
    );
  });

  it("rejects non-http origins and credential-bearing urls", () => {
    expect(() => new RemotePageProvider({ baseUrl: "ftp://127.0.0.1:21" })).toThrow(
      /must be http\(s\)/,
    );
    expect(
      () => new RemotePageProvider({ baseUrl: "http://user:pass@127.0.0.1:8081" }),
    ).toThrow(/must not embed credentials/);
  });

  it("names the target and status on errors, never bodies", async () => {
    const provider = new RemotePageProvider({ baseUrl: origin });
    const error = await provider.fetchContent("missing").catch((e: Error) => e);
    expect(error).toBeInstanceOf(Error);
    expect((error as Error).message).toContain("status 404 for missing");
    // The 404 body ("no such fixture") never reaches the error.
    expect((error as Error).message).not.toContain("no such fixture");
  });

  it("reports a closed origin as a fetch failure for the target", async () => {
    server.closeAllConnections();
    await new Promise<void>((resolve) => server.close(() => resolve()));
    const provider = new RemotePageProvider({ baseUrl: origin });
    await expect(provider.fetchContent("login")).rejects.toThrow(
      /fetch failed for login/,
    );
  });
});

browserSuite("remote page provider integration", () => {
  it("serves fetched controlled content into a real page", async () => {
    const provider = new RemotePageProvider({ baseUrl: origin });
    const worker = new BrowserWorker({
      workerId: "wrk-remote-test",
      provider,
      queue: new ResourceQueue({ maxConcurrent: 1 }),
    });
    try {
      const title = await worker.run({
        taskId: "t-remote",
        target: "login",
        run: ({ page, provider: used }) => {
          expect(used.name).toBe("remote");
          return page.title();
        },
      });
      expect(title).toBe("remote login");
    } finally {
      await worker.stop();
    }
  });

  it("a fetch failure surfaces as a task failure naming the target", async () => {
    server.closeAllConnections();
    await new Promise<void>((resolve) => server.close(() => resolve()));
    const provider = new RemotePageProvider({ baseUrl: origin });
    const worker = new BrowserWorker({
      workerId: "wrk-remote-down",
      provider,
      queue: new ResourceQueue({ maxConcurrent: 1 }),
    });
    try {
      const failure = worker.run({
        taskId: "t-remote-down",
        target: "login",
        run: () => Promise.resolve("unreachable"),
      });
      await expect(failure).rejects.toThrow(/fetch failed for login/);
    } finally {
      await worker.stop();
    }
  });
});
