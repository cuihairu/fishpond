import { createHash } from "node:crypto";
import { describe, expect, it } from "vitest";
import { BrowserWorker, MockPageProvider, browserAvailable } from "./index";
import { ResourceQueue } from "./resource-queue";
import { visible, textMatches, hidden } from "./conditions";
import { captureDom, captureScreenshot, sha256Hex } from "./evidence";
import { PageObject, runOperation } from "./page-object";

// Browser-backed tests run where a chromium binary is installed and
// skip where it is not (CI installs no browsers) — the same skip
// pattern the Rust suite uses for its database tests.
const suite = (await browserAvailable()) ? describe : describe.skip;

const FORM_PAGE = [
  "<html><head><title>mock form</title></head><body>",
  '<h1 data-testid="title">mock form</h1>',
  '<input data-testid="name">',
  '<button data-testid="go" onclick="document.querySelector(\'[data-testid=out]\').textContent = \'done \' + document.querySelector(\'[data-testid=name]\').value">go</button>',
  '<span data-testid="out"></span>',
  "</body></html>",
].join("");

const LIST_PAGE = [
  "<html><body><ul>",
  '<li data-testid="item" data-kind="alpha">alpha</li>',
  '<li data-testid="item" data-kind="beta">beta</li>',
  '<li data-testid="item" data-kind="gamma">gamma</li>',
  "</ul></body></html>",
].join("");

const PROVIDER = MockPageProvider.withFixtures([
  ["form", FORM_PAGE],
  ["list", LIST_PAGE],
]);

function makeWorker(): BrowserWorker {
  return new BrowserWorker({
    workerId: "wkr-f023",
    provider: PROVIDER,
    queue: new ResourceQueue({ maxConcurrent: 2 }),
  });
}

suite("page objects, conditions and evidence", () => {
  it("captures dom and screenshot evidence with matching digests", async () => {
    const worker = makeWorker();
    try {
      const result = await worker.run({
        taskId: "ev-1",
        target: "form",
        run: async (context) => {
          const dom = await captureDom(context.page, "snapshot");
          const shot = await captureScreenshot(context.page, "shot");
          return { dom, shot };
        },
      });
      expect(result.dom.kind).toBe("dom");
      expect(result.dom.dom).toContain("mock form");
      expect(result.dom.size).toBe(Buffer.byteLength(result.dom.dom ?? "", "utf8"));
      expect(result.dom.sha256).toBe(sha256Hex(result.dom.dom ?? ""));

      expect(result.shot.kind).toBe("screenshot");
      expect(result.shot.screenshot?.subarray(0, 4)).toEqual(
        Buffer.from([0x89, 0x50, 0x4e, 0x47]),
        "payload is a PNG",
      );
      expect(result.shot.size).toBe(result.shot.screenshot?.length);
      expect(result.shot.sha256).toBe(
        createHash("sha256").update(result.shot.screenshot ?? Buffer.alloc(0)).digest("hex"),
      );
    } finally {
      await worker.stop();
    }
  });

  it("runs a gated operation end to end with evidence", async () => {
    const worker = makeWorker();
    try {
      const outcome = await worker.run({
        taskId: "op-1",
        target: "form",
        run: (context) =>
          runOperation(context.page, {
            name: "submit-form",
            pre: [visible('[data-testid="go"]')],
            post: [textMatches('[data-testid="out"]', /done kui/)],
            evidence: [{ name: "after", kind: "dom" }],
            run: async (page) => {
              await page.fill('[data-testid="name"]', "kui");
              await page.click('[data-testid="go"]');
              return page.textContent('[data-testid="out"]');
            },
          }),
      });
      expect(outcome.ok).toBe(true);
      expect(outcome.stage).toBeNull();
      expect(outcome.value).toBe("done kui");
      expect(outcome.pre).toEqual([
        { name: "visible [data-testid=\"go\"]", ok: true },
      ]);
      expect(outcome.post.every((c) => c.ok)).toBe(true);
      expect(outcome.evidence).toHaveLength(1);
      expect(outcome.evidence[0]?.dom).toContain("done kui");
    } finally {
      await worker.stop();
    }
  });

  it("short-circuits when a pre-condition fails", async () => {
    const worker = makeWorker();
    try {
      let actionRan = false;
      const outcome = await worker.run({
        taskId: "op-2",
        target: "form",
        run: (context) =>
          runOperation(context.page, {
            name: "gated",
            pre: [visible('[data-testid="missing"]')],
            run: async () => {
              actionRan = true;
              return "never";
            },
          }),
      });
      expect(outcome.ok).toBe(false);
      expect(outcome.stage).toBe("pre");
      expect(actionRan).toBe(false);
      expect(outcome.post).toEqual([]);
      expect(outcome.evidence).toEqual([]);
      expect(outcome.pre[0]?.ok).toBe(false);
      expect(outcome.pre[0]?.name).toContain("missing");
    } finally {
      await worker.stop();
    }
  });

  it("captures evidence and names the error when the action throws", async () => {
    const worker = makeWorker();
    try {
      const outcome = await worker.run({
        taskId: "op-3",
        target: "form",
        run: (context) =>
          runOperation(context.page, {
            name: "failing",
            evidence: [{ name: "broken", kind: "dom" }],
            run: async () => {
              throw new Error("boom: selector refused");
            },
          }),
      });
      expect(outcome.ok).toBe(false);
      expect(outcome.stage).toBe("action");
      expect(outcome.error).toBe("boom: selector refused");
      expect(outcome.value).toBeUndefined();
      expect(outcome.post).toEqual([]);
      expect(outcome.evidence).toHaveLength(1);
      expect(outcome.evidence[0]?.name).toBe("broken");
      expect(outcome.evidence[0]?.dom).toContain("mock form");
    } finally {
      await worker.stop();
    }
  });

  it("marks the operation failed when a post-condition breaks", async () => {
    const worker = makeWorker();
    try {
      const outcome = await worker.run({
        taskId: "op-4",
        target: "form",
        run: (context) =>
          runOperation(context.page, {
            name: "post-gated",
            post: [hidden('[data-testid="out"]')],
            run: async (page) => page.textContent('[data-testid="title"]'),
          }),
      });
      expect(outcome.ok).toBe(false);
      expect(outcome.stage).toBe("post");
      expect(outcome.value).toBe("mock form");
      expect(outcome.post[0]?.ok).toBe(false);
      expect(outcome.post[0]?.name).toContain("hidden");
    } finally {
      await worker.stop();
    }
  });

  it("exposes page-object primitives over the served page", async () => {
    const worker = makeWorker();
    try {
      const result = await worker.run({
        taskId: "po-1",
        target: "list",
        run: async (context) => {
          const page = new PageObject(context.page);
          return {
            items: await page.count('[data-testid="item"]'),
            // Playwright is strict: text/attribute probes need a
            // unique selector; count() is the multi-match tool.
            first: await page.textContent('[data-kind="alpha"]'),
            kind: await page.attribute('[data-kind="beta"]', "data-kind"),
            listVisible: await page.isVisible("ul"),
            missingVisible: await page.isVisible('[data-testid="nope"]'),
          };
        },
      });
      expect(result.items).toBe(3);
      expect(result.first).toBe("alpha");
      expect(result.kind).toBe("beta");
      expect(result.listVisible).toBe(true);
      expect(result.missingVisible).toBe(false);
    } finally {
      await worker.stop();
    }
  });
});
