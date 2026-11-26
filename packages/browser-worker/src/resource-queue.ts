/**
 * Resource queue (F021): bounds how many browser resources (contexts,
 * pages) run at once, starts tasks strictly in call order, and gives
 * the caller a backpressure signal (F024's 浏览器背压 builds on it).
 *
 * The queue never swallows task errors: each task's failure rejects
 * only that task's promise, and the slot frees for the next task.
 */
export interface ResourceQueueOptions {
  /** Maximum number of tasks running at the same time (≥ 1). */
  readonly maxConcurrent: number;
  /**
   * Maximum number of tasks allowed in flight (running + waiting)
   * before new submissions are rejected. Unbounded by default.
   */
  readonly maxPending?: number;
}

/** Submitted when the queue is already at its pending capacity. */
export class ResourceQueueFull extends Error {
  /** The configured in-flight capacity. */
  readonly capacity: number;

  constructor(capacity: number) {
    super(`resource queue is full (capacity ${capacity})`);
    this.name = "ResourceQueueFull";
    this.capacity = capacity;
  }
}

export class ResourceQueue {
  private readonly maxConcurrent: number;
  private readonly maxPending: number;
  private active = 0;
  private waiting = 0;
  /** FIFO of accepted tasks waiting for a slot (resolve = start). */
  private readonly waiters: Array<() => void> = [];
  private readonly idleWaiters: Array<() => void> = [];

  constructor(options: ResourceQueueOptions) {
    if (!Number.isInteger(options.maxConcurrent) || options.maxConcurrent < 1) {
      throw new RangeError("maxConcurrent must be an integer ≥ 1");
    }
    if (
      options.maxPending !== undefined &&
      (!Number.isInteger(options.maxPending) || options.maxPending < 1)
    ) {
      throw new RangeError("maxPending must be an integer ≥ 1");
    }
    this.maxConcurrent = options.maxConcurrent;
    this.maxPending = options.maxPending ?? Number.POSITIVE_INFINITY;
  }

  /** Tasks currently running (slot already acquired). */
  get running(): number {
    return this.active;
  }

  /** Tasks accepted but not yet started. */
  get pending(): number {
    return this.waiting;
  }

  /**
   * Runs `task` when a slot frees, after every earlier submission has
   * started. Resolves with the task's result; rejects with the task's
   * error (or [`ResourceQueueFull`] when the queue is at capacity).
   */
  run<T>(task: () => Promise<T>): Promise<T> {
    if (this.active + this.waiting >= this.maxPending) {
      return Promise.reject(new ResourceQueueFull(this.maxPending));
    }
    const canStart = new Promise<void>((resolve) => {
      this.waiters.push(resolve);
    });
    this.waiting += 1;
    this.tryPromote();
    return canStart.then(() => {
      return task().finally(() => {
        this.active -= 1;
        this.tryPromote();
        this.notifyIdle();
      });
    });
  }

  /** Resolves once no task is running or waiting. */
  awaitIdle(): Promise<void> {
    if (this.active === 0 && this.waiting === 0) {
      return Promise.resolve();
    }
    return new Promise((resolve) => {
      this.idleWaiters.push(resolve);
    });
  }

  /**
   * Hands free slots to the earliest waiters. The slot (active count)
   * is acquired synchronously here, so two back-to-back submissions
   * can never both squeeze into the last slot via microtask ordering.
   */
  private tryPromote(): void {
    while (this.active < this.maxConcurrent && this.waiters.length > 0) {
      this.waiters.shift()!();
      this.active += 1;
      this.waiting -= 1;
    }
  }

  private notifyIdle(): void {
    if (this.active === 0 && this.waiting === 0) {
      for (const resolve of this.idleWaiters.splice(0)) {
        resolve();
      }
    }
  }
}
