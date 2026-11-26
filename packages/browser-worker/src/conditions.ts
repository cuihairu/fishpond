/**
 * Pre/post conditions (F023 / T08): named, page-checking predicates
 * that gate an operation. Conditions are cheap, non-waiting probes —
 * the page is already prepared when they run (the provider served it),
 * so they assert state rather than race for it.
 *
 * Every outcome names its condition, so a failed gate reads as
 * "condition `submit visible` did not hold" instead of a bare boolean.
 */
import type { Page } from "playwright";

/** A named predicate over the page's current state. */
export interface Condition {
  /** Stable name (appears in outcomes and diagnostics). */
  readonly name: string;
  /** Checks the page; must not wait or retry. */
  check(page: Page): Promise<boolean>;
}

/** One evaluated condition. */
export interface ConditionOutcome {
  /** The condition's name. */
  readonly name: string;
  /** Whether it held. */
  readonly ok: boolean;
}

/** Evaluates every condition against the page, preserving order. */
export async function evaluateConditions(
  page: Page,
  conditions: readonly Condition[],
): Promise<ConditionOutcome[]> {
  const outcomes: ConditionOutcome[] = [];
  for (const condition of conditions) {
    outcomes.push({ name: condition.name, ok: await condition.check(page) });
  }
  return outcomes;
}

/** All held? */
export function allOk(outcomes: readonly ConditionOutcome[]): boolean {
  return outcomes.every((outcome) => outcome.ok);
}

/** The condition requires the selector to resolve and be visible. */
export function visible(selector: string): Condition {
  return {
    name: `visible ${selector}`,
    async check(page: Page): Promise<boolean> {
      return page.locator(selector).isVisible();
    },
  };
}

/** The condition requires the selector to be absent or hidden. */
export function hidden(selector: string): Condition {
  return {
    name: `hidden ${selector}`,
    async check(page: Page): Promise<boolean> {
      return !page.locator(selector).isVisible();
    },
  };
}

/**
 * The condition requires the element's text to match `pattern` (an
 * absent element never matches).
 */
export function textMatches(selector: string, pattern: RegExp): Condition {
  return {
    name: `text of ${selector} matches ${pattern}`,
    async check(page: Page): Promise<boolean> {
      const text = await page.locator(selector).textContent();
      return text !== null && pattern.test(text);
    },
  };
}
