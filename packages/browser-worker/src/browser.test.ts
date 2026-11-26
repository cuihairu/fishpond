import { describe, expect, it } from "vitest";
import { BrowserWorker, MockPageProvider, browserAvailable } from "./index";
import { ResourceQueue } from "./resource-queue";

// Browser-backed tests run where a chromium binary is installed and
// skip where it is not (CI installs no browsers) — the same skip
// pattern the Rust suite uses for its database tests.
const suite = (await browserAvailable()) ? describe : describe.skip;

const FIXTURES: ReadonlyArray<readonly [string, string]> = [
  [
    "login",
    [
      "<html><head><title>mock login</title></head><body>",
      '<h1 data-testid="title">mock login</h1>',
      '<input data-testid="username">',
      '<button data-testid="submit">go</button>',
      "</body></html>",
    ].join(""),
  ],
  [
    "list",
    [
      "<html><body><ul>",
      '<li data-testid="item">alpha</li>',
      '<li data-testid="item">beta</li>',
      "</ul></body></html>",
    ].join(""),
  ],
];

function workerFor(
  workerId: string,
  queue: ResourceQueue,
): BrowserWorker {
  return new BrowserWorker({
    workerId,
    provider: MockPageProvider.withFixtures(FIXTURES),
    queue,
  });
}

suite("MockPageProvider", () => {
  it("serves fixture html into the page", async () => {
    const worker = workerFor("w-provider", new ResourceQueue({ maxConcurrent: 1 }));
    try {
      const title = await worker.run({
        taskId: "t1",
        target: "login",
        run: ({ page }) => page.textContent('[data-testid="title"]'),
      });
      expect(title).toBe("mock login");
    } finally {
      await worker.stop();
    }
  });

  it("names the missing fixture instead of leaking page content", async () => {
    const worker = workerFor("w-provider", new ResourceQueue({ maxConcurrent: 1 }));
    try {
      const failure = worker.run({
        taskId: "t2",
        target: "no-such-fixture",
        run: () => Promise.resolve("unreachable"),
      });
      await expect(failure).rejects.toThrow("no fixture named no-such-fixture");
    } finally {
      await worker.stop();
    }
  });
});

suite("BrowserWorker", () => {
  it("exposes the worker id and task context", async () => {
    const worker = workerFor("w-ctx", new ResourceQueue({ maxConcurrent: 1 }));
    try {
      const seen = await worker.run({
        taskId: "task-42",
        target: "login",
        run: ({ page, provider, workerId, taskId }) =>
          page.textContent('[data-testid="title"]').then((title) => ({
            title,
            providerName: provider.name,
            workerId,
            taskId,
          })),
      });
      expect(seen).toEqual({
        title: "mock login",
        providerName: "mock",
        workerId: "w-ctx",
        taskId: "task-42",
      });
    } finally {
      await worker.stop();
    }
  });

  it("keeps concurrent tasks within the queue bound", async () => {
    const queue = new ResourceQueue({ maxConcurrent: 2 });
    const worker = workerFor("w-bound", queue);
    try {
      let active = 0;
      let peak = 0;
      const runs = Array.from({ length: 4 }, (_, i) =>
        worker.run({
          taskId: `t-${i}`,
          target: "list",
          run: async ({ page }) => {
            await page.textContent("ul");
            active += 1;
            peak = Math.max(peak, active);
            // Hold every task open long enough for all four bodies to
            // run under the bound of two.
            await new Promise((r) => setTimeout(r, 20));
            active -= 1;
            return i;
          },
        }),
      );
      const results = await Promise.all(runs);
      expect(peak).toBeLessThanOrEqual(2);
      expect(results).toEqual([0, 1, 2, 3]);
    } finally {
      await worker.stop();
    }
  });

  it("a failing task does not poison the worker", async () => {
    const worker = workerFor("w-fail", new ResourceQueue({ maxConcurrent: 1 }));
    try {
      const boom = worker.run({
        taskId: "bad",
        target: "login",
        run: () => {
          throw new Error("boom");
        },
      });
      await expect(boom).rejects.toThrow("boom");
      const next = await worker.run({
        taskId: "good",
        target: "list",
        run: ({ page }) => page.textContent("li"),
      });
      expect(next).toBe("alpha");
    } finally {
      await worker.stop();
    }
  });

  it("stop is idempotent and the worker relaunches on demand", async () => {
    const worker = workerFor("w-stop", new ResourceQueue({ maxConcurrent: 1 }));
    const first = await worker.run({
      taskId: "t1",
      target: "login",
      run: ({ page }) => page.title(),
    });
    expect(first).toBe("mock login");
    await worker.stop();
    await worker.stop();
    const second = await worker.run({
      taskId: "t2",
      target: "login",
      run: ({ page }) => page.title(),
    });
    expect(second).toBe("mock login");
    await worker.stop();
  });
});
