// F035: the presentation rules — UNKNOWN renders as an observational
// state (never silently as healthy), actions follow the domain state
// machines exactly, and permission views say what a key may do
// instead of hiding failures.

import { describe, expect, it } from "vitest";

import {
  accountActions,
  accountStateView,
  describeError,
  formatTime,
  operationStateView,
  permissionView,
  sessionActions,
  sessionStateView,
} from "../src/present.js";
import { ApiError } from "../src/api.js";

describe("state views", () => {
  it("shows UNKNOWN as an observational warning, not health", () => {
    const view = accountStateView("UNKNOWN");
    expect(view.tone).toBe("observe");
    expect(view.observation).toBeTruthy();
    expect(accountStateView("ONLINE").tone).toBe("healthy");
    expect(accountStateView("OFFLINE").tone).toBe("warn");
    expect(accountStateView("DISABLED").tone).toBe("dead");
  });

  it("rejects state names the domain does not define", () => {
    expect(() => accountStateView("MAYBE")).toThrow();
  });

  it("shows terminal session states as dead and TAKEN_OVER as busy", () => {
    expect(sessionStateView("AUTHORIZED").tone).toBe("healthy");
    expect(sessionStateView("TAKEN_OVER").tone).toBe("busy");
    expect(sessionStateView("EXPIRED").tone).toBe("dead");
    expect(sessionStateView("REVOKED").tone).toBe("dead");
    expect(sessionStateView("FAILED").tone).toBe("dead");
    expect(() => sessionStateView("GHOST")).toThrow();
  });

  it("shows operation terminals as done, not failed-alive", () => {
    expect(operationStateView("SUCCEEDED").tone).toBe("done");
    expect(operationStateView("FAILED").tone).toBe("done");
    expect(operationStateView("CANCELLED").tone).toBe("done");
    expect(operationStateView("PENDING").tone).toBe("busy");
    expect(operationStateView("RUNNING").tone).toBe("busy");
    expect(() => operationStateView("LOST")).toThrow();
  });
});

describe("available actions follow the domain machines", () => {
  it("account actions match the account table", () => {
    expect(accountActions("ONLINE")).toEqual(["OFFLINE", "UNKNOWN", "DISABLED"]);
    expect(accountActions("OFFLINE")).toEqual(["ONLINE", "UNKNOWN", "DISABLED"]);
    expect(accountActions("UNKNOWN")).toEqual(["ONLINE", "OFFLINE", "DISABLED"]);
    expect(accountActions("DISABLED")).toEqual([]);
  });

  it("session actions match the auth-session table and the REST verbs", () => {
    expect(sessionActions("PENDING")).toEqual(["confirm", "authorize", "expire", "revoke", "fail"]);
    expect(sessionActions("AWAITING_CONFIRMATION")).toEqual([
      "authorize",
      "expire",
      "revoke",
      "fail",
    ]);
    expect(sessionActions("AUTHORIZED")).toEqual(["take-over", "expire", "revoke", "fail"]);
    expect(sessionActions("TAKEN_OVER")).toEqual(["resume", "expire", "revoke"]);
    expect(sessionActions("EXPIRED")).toEqual([]);
    expect(sessionActions("REVOKED")).toEqual([]);
    expect(sessionActions("FAILED")).toEqual([]);
  });
});

describe("permission views", () => {
  it("names what a full key may do", () => {
    const view = permissionView(["sessions:read", "sessions:write", "sessions:takeover"]);
    expect(view.can.readSessions).toBe(true);
    expect(view.can.writeSessions).toBe(true);
    expect(view.can.takeover).toBe(true);
    expect(view.missing).toEqual([]);
  });

  it("lists exactly what a narrow key is missing", () => {
    const view = permissionView(["sessions:read"]);
    expect(view.can.readSessions).toBe(true);
    expect(view.can.writeSessions).toBe(false);
    expect(view.can.takeover).toBe(false);
    expect(view.missing).toEqual(["sessions:write", "sessions:takeover"]);
  });

  it("never invents permissions for an empty key", () => {
    const view = permissionView([]);
    expect(Object.values(view.can).every((allowed) => allowed === false)).toBe(true);
    expect(view.missing.length).toBeGreaterThan(0);
  });
});

describe("operator-facing text", () => {
  it("formats instants readably", () => {
    expect(formatTime(0)).toContain("1970");
  });

  it("describes api problems as data, with the server's detail", () => {
    const problem = new ApiError(
      { type: "https://fishpond.dev/problems/scope-denied", title: "scope denied", status: 403, detail: "需要 sessions:write" },
      "forbidden",
    );
    const text = describeError(problem);
    expect(text).toContain("权限");
    expect(text).toContain("sessions:write");
  });

  it("falls back to the raw message for plain errors", () => {
    expect(describeError(new Error("boom"))).toBe("boom");
    expect(describeError("weird")).toContain("weird");
  });
});
