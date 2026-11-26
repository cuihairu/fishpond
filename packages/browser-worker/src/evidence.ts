/**
 * Result evidence (F023 / T08): what a page operation leaves behind
 * so its outcome can be audited without re-running it — a DOM snapshot
 * (the serialized HTML) and/or a screenshot (PNG bytes).
 *
 * Evidence is self-describing: every record carries its payload's
 * SHA-256 and byte size, so a consumer can verify integrity without
 * understanding the format. Payloads stay in memory; persisting them
 * (to the operation attempt record, F014) is the transport's job.
 */
import { createHash } from "node:crypto";
import type { Page } from "playwright";

/** The two evidence forms captured today. */
export type EvidenceKind = "dom" | "screenshot";

/** One captured piece of evidence (payload inline, digest attached). */
export interface Evidence {
  /** Caller-chosen name (stable within one operation). */
  readonly name: string;
  /** Which form this is. */
  readonly kind: EvidenceKind;
  /** Payload size in bytes (DOM = UTF-8 length, screenshot = PNG). */
  readonly size: number;
  /** SHA-256 of the payload bytes, hex. */
  readonly sha256: string;
  /** The serialized HTML (kind `dom` only). */
  readonly dom?: string;
  /** The PNG bytes (kind `screenshot` only). */
  readonly screenshot?: Buffer;
}

/** SHA-256 over the payload bytes, hex-encoded. */
export function sha256Hex(data: Buffer | string): string {
  const bytes = typeof data === "string" ? Buffer.from(data, "utf8") : data;
  return createHash("sha256").update(bytes).digest("hex");
}

/** Captures the page's DOM snapshot as evidence. */
export async function captureDom(page: Page, name: string): Promise<Evidence> {
  const dom = await page.content();
  const bytes = Buffer.from(dom, "utf8");
  return {
    name,
    kind: "dom",
    size: bytes.length,
    sha256: sha256Hex(bytes),
    dom,
  };
}

/** Captures a PNG screenshot of the page as evidence. */
export async function captureScreenshot(page: Page, name: string): Promise<Evidence> {
  const screenshot = await page.screenshot({ type: "png" });
  return {
    name,
    kind: "screenshot",
    size: screenshot.length,
    sha256: sha256Hex(screenshot),
    screenshot,
  };
}

/** Which evidence to capture for an operation (name + form). */
export interface EvidenceRequest {
  /** Evidence name within the operation's outcome. */
  readonly name: string;
  /** Which form to capture. */
  readonly kind: EvidenceKind;
}

/** Captures every requested piece of evidence, in request order. */
export async function captureRequested(
  page: Page,
  requests: readonly EvidenceRequest[],
): Promise<Evidence[]> {
  const captured: Evidence[] = [];
  for (const request of requests) {
    captured.push(
      request.kind === "dom"
        ? await captureDom(page, request.name)
        : await captureScreenshot(page, request.name),
    );
  }
  return captured;
}
