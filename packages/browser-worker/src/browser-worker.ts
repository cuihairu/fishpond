/**
 * Browser worker (F021): Playwright lifecycle wrapped in the resource
 * queue. One shared browser, one fresh context per task (crash and
 * state isolation), teardown that never masks task errors.
 *
 * The worker is the shell F023 builds page-object operations on; the
 * control-plane enrolment (F020's signed service identity) lands with
 * the REST transport (F025) — there is nothing to authenticate to
 * before that transport exists.
 */
import * as fs from "node:fs";
import { chromium, type Browser, type Page } from "playwright";
import type { PageProvider } from "./page-provider";
import type { ResourceQueue } from "./resource-queue";

export interface BrowserWorkerOptions {
  /** Stable id of this worker instance (diagnostics only). */
  readonly workerId: string;
  /** Where task pages come from (the mock provider in CI). */
  readonly provider: PageProvider;
  /** The queue bounding concurrent tasks. */
  readonly queue: ResourceQueue;
  /** Launch headless (default true). */
  readonly headless?: boolean;
}

/** What a task sees while it runs. */
export interface TaskContext {
  readonly page: Page;
  readonly provider: PageProvider;
  readonly workerId: string;
  readonly taskId: string;
}

/** One unit of browser work: a fixture target plus its behavior. */
export interface WorkerTask<R> {
  readonly taskId: string;
  /** Provider target (fixture name for the mock provider). */
  readonly target: string;
  run(context: TaskContext): Promise<R>;
}

export class BrowserWorker {
  private readonly options: BrowserWorkerOptions;
  private browser: Browser | null = null;

  constructor(options: BrowserWorkerOptions) {
    this.options = options;
  }

  /** The worker's stable id. */
  get workerId(): string {
    return this.options.workerId;
  }

  /**
   * Runs `task` in a fresh browser context, bounded by the queue.
   * Context teardown happens even when the task fails, and the
   * failure rejects only this task.
   */
  run<R>(task: WorkerTask<R>): Promise<R> {
    return this.options.queue.run(async () => {
      const browser = await this.ensureBrowser();
      const context = await browser.newContext();
      try {
        const page = await context.newPage();
        await this.options.provider.serve(page, task.target);
        return await task.run({
          page,
          provider: this.options.provider,
          workerId: this.options.workerId,
          taskId: task.taskId,
        });
      } finally {
        // Teardown never masks the task's error and never wedges the
        // queue slot: close failures are absorbed.
        await context.close().catch(() => undefined);
      }
    });
  }

  /** Closes the shared browser (idempotent). */
  async stop(): Promise<void> {
    const browser = this.browser;
    this.browser = null;
    if (browser !== null) {
      await browser.close().catch(() => undefined);
    }
  }

  private async ensureBrowser(): Promise<Browser> {
    if (this.browser === null || !this.browser.isConnected()) {
      this.browser = await chromium.launch({
        headless: this.options.headless ?? true,
      });
    }
    return this.browser;
  }
}

/**
 * Whether a Playwright chromium binary is installed locally. Browser
 * tests skip when it is not (CI installs no browsers; the Rust test
 * suite skips its database tests the same way).
 */
export async function browserAvailable(): Promise<boolean> {
  try {
    const executable = chromium.executablePath();
    return fs.existsSync(executable);
  } catch {
    return false;
  }
}
