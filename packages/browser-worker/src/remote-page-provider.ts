/**
 * Controlled remote page provider (F044): the external-controlled
 * face of the PageProvider SPI — content comes from a separate
 * controlled service over HTTP instead of in-process fixture
 * strings, while tests stay deterministic (a loopback fixture
 * server) and the deployment stays offline of the platform.
 *
 * Red line: real platform URLs are F032+ territory behind the
 * authorization red line. This provider only ever fetches from the
 * operator-configured base URL — a task `target` resolves to one
 * path segment under that origin and can never change the host —
 * and error messages name the target and status, never response
 * bodies.
 */
import type { Page } from "playwright";
import type { PageProvider } from "./page-provider";

export class RemotePageProvider implements PageProvider {
  readonly name: string;

  private readonly baseUrl: string;

  private readonly fetchImpl: typeof fetch;

  constructor(options: {
    /** Provider name (default "remote"). */
    name?: string;
    /** Controlled content origin, e.g. `http://127.0.0.1:8081`. */
    baseUrl: string;
    /** Fetch implementation (injectable for tests). */
    fetchImpl?: typeof fetch;
  }) {
    const parsed = new URL(options.baseUrl);
    if (parsed.protocol !== "http:" && parsed.protocol !== "https:") {
      throw new Error(`provider base url must be http(s), got ${parsed.protocol}`);
    }
    if (parsed.username || parsed.password) {
      // A controlled origin carries no credentials in its URL; the
      // authorization story is network placement, not userinfo.
      throw new Error("provider base url must not embed credentials");
    }
    this.name = options.name ?? "remote";
    this.baseUrl = parsed.toString().replace(/\/$/, "");
    this.fetchImpl = options.fetchImpl ?? fetch;
  }

  /** Absolute URL for a target: one path segment, host never changes. */
  urlFor(target: string): string {
    return `${this.baseUrl}/${encodeURIComponent(target)}`;
  }

  /** Fetches the page content for `target` from the controlled origin. */
  async fetchContent(target: string): Promise<string> {
    let response: Response;
    try {
      response = await this.fetchImpl(this.urlFor(target));
    } catch (error) {
      // The failure names the task target and the transport error;
      // no origin credentials exist to leak and bodies are unread.
      throw new Error(
        `remote provider fetch failed for ${target}: ${(error as Error).message}`,
      );
    }
    if (!response.ok) {
      throw new Error(`remote provider got status ${response.status} for ${target}`);
    }
    return response.text();
  }

  async serve(page: Page, target: string): Promise<void> {
    const html = await this.fetchContent(target);
    await page.setContent(html);
  }
}
