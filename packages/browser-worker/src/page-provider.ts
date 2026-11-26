/**
 * Page provider SPI (F021): where a worker's pages come from.
 *
 * Real providers navigate platform URLs — that is F032+ territory
 * behind the authorization red line. The mock provider is the default
 * and the only CI target: it serves deterministic local HTML fixtures
 * straight into the page, so tests drive a real browser without any
 * network access.
 */
import type { Page } from "playwright";

/** Supplies pages (or page content) for worker tasks. */
export interface PageProvider {
  /** Provider name (for task logs and diagnostics). */
  readonly name: string;
  /** Prepares `page` for the named fixture/task target. */
  serve(page: Page, target: string): Promise<void>;
}

/** Serves deterministic local HTML fixtures into pages. */
export class MockPageProvider implements PageProvider {
  readonly name = "mock";
  private readonly fixtures: ReadonlyMap<string, string>;

  constructor(fixtures: ReadonlyMap<string, string>) {
    this.fixtures = fixtures;
  }

  /** Builds a provider from `[name, html]` entries. */
  static withFixtures(entries: ReadonlyArray<readonly [string, string]>): MockPageProvider {
    return new MockPageProvider(new Map(entries));
  }

  async serve(page: Page, target: string): Promise<void> {
    const html = this.fixtures.get(target);
    if (html === undefined) {
      // Error text names the fixture — never page content.
      throw new Error(`mock provider has no fixture named ${target}`);
    }
    // setContent keeps everything offline and deterministic: no
    // navigation, no network, no timing races.
    await page.setContent(html, { waitUntil: "load" });
  }
}
