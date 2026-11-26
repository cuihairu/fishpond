import { describe, expect, it } from "vitest";
import { ResourceQueue, ResourceQueueFull } from "./resource-queue";

interface Deferred {
  promise: Promise<void>;
  resolve: () => void;
}

function deferred(): Deferred {
  let resolve!: () => void;
  const promise = new Promise<void>((r) => {
    resolve = r;
  });
  return { promise, resolve };
}

describe("ResourceQueue", () => {
  it("rejects invalid bounds", () => {
    expect(() => new ResourceQueue({ maxConcurrent: 0 })).toThrow(RangeError);
    expect(() => new ResourceQueue({ maxConcurrent: 1.5 })).toThrow(RangeError);
    expect(() => new ResourceQueue({ maxConcurrent: 2, maxPending: 0 })).toThrow(RangeError);
  });

  it("never runs more than maxConcurrent tasks", async () => {
    const queue = new ResourceQueue({ maxConcurrent: 2 });
    const release = deferred();
    let peak = 0;
    let active = 0;
    const runs = Array.from({ length: 5 }, () =>
      queue.run(async () => {
        active += 1;
        peak = Math.max(peak, active);
        await release.promise;
        active -= 1;
        return active;
      }),
    );
    expect(queue.pending).toBe(3);
    release.resolve();
    const results = await Promise.all(runs);
    await queue.awaitIdle();
    expect(peak).toBe(2);
    expect(queue.running).toBe(0);
    expect(results).toHaveLength(5);
  });

  it("starts tasks strictly in submission order", async () => {
    const queue = new ResourceQueue({ maxConcurrent: 1 });
    const allStarted = deferred();
    const started: string[] = [];
    const first = queue.run(async () => {
      started.push("first");
      await allStarted.promise;
      return "first-done";
    });
    const second = queue.run(async () => {
      started.push("second");
      return "second-done";
    });
    const third = queue.run(async () => {
      started.push("third");
      return "third-done";
    });
    // The blocked first task must hold the later submissions: none of
    // them may start before it finishes.
    await new Promise((r) => setTimeout(r, 10));
    expect(started).toEqual(["first"]);
    allStarted.resolve();
    expect(await first).toBe("first-done");
    expect(await second).toBe("second-done");
    expect(await third).toBe("third-done");
    expect(started).toEqual(["first", "second", "third"]);
  });

  it("a failing task rejects its caller only and frees the slot", async () => {
    const queue = new ResourceQueue({ maxConcurrent: 1 });
    const boom = queue.run(async () => {
      throw new Error("task blew up");
    });
    await expect(boom).rejects.toThrow("task blew up");
    const next = queue.run(async () => "still works");
    expect(await next).toBe("still works");
    await queue.awaitIdle();
    expect(queue.running).toBe(0);
    expect(queue.pending).toBe(0);
  });

  it("rejects submissions beyond maxPending", async () => {
    const queue = new ResourceQueue({ maxConcurrent: 1, maxPending: 2 });
    const release = deferred();
    void queue.run(async () => {
      await release.promise;
      return 1;
    });
    void queue.run(async () => 2);
    expect(queue.pending).toBe(1);
    await expect(queue.run(async () => 3)).rejects.toThrow(ResourceQueueFull);
    release.resolve();
    await queue.awaitIdle();
  });

  it("awaitIdle resolves immediately on an empty queue", async () => {
    const queue = new ResourceQueue({ maxConcurrent: 1 });
    await queue.awaitIdle();
  });
});
