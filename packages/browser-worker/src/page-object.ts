/**
 * Page-object operations (F023 / T08): the typed primitives a task
 * drives a page with, and `runOperation` — the wrapper that gates an
 * action behind pre-conditions, verifies post-conditions, and collects
 * the requested evidence around it.
 *
 * An operation runs in one fixed order: pre → action → evidence →
 * post. A failed stage short-circuits (the action never runs behind a
 * failed pre; post still runs after a successful action even when it
 * then fails the operation), and every outcome records how far it got
 * (`stage`), so the caller can tell "the gate refused" from "the page
 * broke mid-action". Evidence is captured even when the action throws
 * — the broken state is exactly what an audit wants to see. Error
 * fields carry messages only, never page content.
 */
import type { Page } from "playwright";
import type { Condition, ConditionOutcome } from "./conditions";
import { allOk, evaluateConditions } from "./conditions";
import type { Evidence, EvidenceRequest } from "./evidence";
import { captureRequested } from "./evidence";

/** Typed, awaitable primitives over one page (the page-object base). */
export class PageObject {
  /** The page this object drives. */
  readonly page: Page;

  constructor(page: Page) {
    this.page = page;
  }

  /** Clicks the first element matching `selector`. */
  async click(selector: string): Promise<void> {
    await this.page.locator(selector).click();
  }

  /** Fills the first input matching `selector` with `value`. */
  async fill(selector: string, value: string): Promise<void> {
    await this.page.locator(selector).fill(value);
  }

  /** The element's text content (null when absent). */
  async textContent(selector: string): Promise<string | null> {
    return this.page.locator(selector).textContent();
  }

  /** The element's named attribute (null when absent). */
  async attribute(selector: string, attr: string): Promise<string | null> {
    return this.page.locator(selector).getAttribute(attr);
  }

  /** Whether the selector currently resolves to something visible. */
  async isVisible(selector: string): Promise<boolean> {
    return this.page.locator(selector).isVisible();
  }

  /** How many elements match. */
  async count(selector: string): Promise<number> {
    return this.page.locator(selector).count();
  }
}

/** Where an operation stopped (null when it completed cleanly). */
export type OperationStage = "pre" | "action" | "post";

/** What one operation produced, whatever way it went. */
export interface OperationOutcome<R> {
  /** Whether pre held, the action succeeded and post held. */
  readonly ok: boolean;
  /** The stage that failed (null when ok). */
  readonly stage: OperationStage | null;
  /** The action's return value (set once the action succeeded). */
  readonly value?: R;
  /** The failure's message (set when stage is `action`). */
  readonly error?: string;
  /** Pre-condition outcomes (always evaluated). */
  readonly pre: readonly ConditionOutcome[];
  /** Post-condition outcomes (empty unless the action succeeded). */
  readonly post: readonly ConditionOutcome[];
  /** Captured evidence (empty unless the action ran). */
  readonly evidence: readonly Evidence[];
}

/** An operation: the action plus its gates and its evidence plan. */
export interface OperationOptions<R> {
  /** Operation name (diagnostics). */
  readonly name: string;
  /** Conditions that must hold before the action runs. */
  readonly pre?: readonly Condition[];
  /** Conditions that must hold after the action ran. */
  readonly post?: readonly Condition[];
  /** Evidence to capture after the action (even when it throws). */
  readonly evidence?: readonly EvidenceRequest[];
  /** The action itself; receives a fresh PageObject over the page. */
  run(pageObject: PageObject): Promise<R>;
}

/** Runs one gated, evidence-collecting operation over `page`. */
export async function runOperation<R>(
  page: Page,
  options: OperationOptions<R>,
): Promise<OperationOutcome<R>> {
  const pre = await evaluateConditions(page, options.pre ?? []);
  if (!allOk(pre)) {
    return { ok: false, stage: "pre", pre, post: [], evidence: [] };
  }

  const pageObject = new PageObject(page);
  let action: { ok: true; value: R } | { ok: false; error: string };
  try {
    action = { ok: true, value: await options.run(pageObject) };
  } catch (err) {
    action = { ok: false, error: err instanceof Error ? err.message : String(err) };
  }
  const evidence = await captureRequested(page, options.evidence ?? []);
  if (!action.ok) {
    return { ok: false, stage: "action", error: action.error, pre, post: [], evidence };
  }

  const post = await evaluateConditions(page, options.post ?? []);
  if (!allOk(post)) {
    return { ok: false, stage: "post", value: action.value, pre, post, evidence };
  }
  return { ok: true, stage: null, value: action.value, pre, post, evidence };
}
