/**
 * Browser Worker placeholder (F002).
 *
 * F021 replaces this with the real Playwright-driven worker: page-object
 * operations, resource queue and result evidence collection.
 */
export interface BrowserWorkerOptions {
  /** Identifier of the worker instance. */
  readonly id: string;
}

/** Creates a placeholder browser worker descriptor. */
export function createBrowserWorker(options: BrowserWorkerOptions): string {
  return `browser-worker:${options.id}:placeholder`;
}
