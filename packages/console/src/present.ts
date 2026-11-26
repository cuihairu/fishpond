// F035: presentation rules — raw domain state tokens become tones,
// the action lists mirror the domain transition tables in
// crates/domain/src/state.rs exactly (rest verbs per F025), and a
// key's scope list is rendered as what it may and may not do.
// UNKNOWN is observational: it gets its own tone and an explicit
// note, never a silent pass as healthy.

import { ApiError, type ProblemKind } from "./api.js";

export type Tone = "healthy" | "warn" | "dead" | "busy" | "done" | "observe";

const KIND_LABELS: Record<ProblemKind, string> = {
  forbidden: "权限不足",
  missing: "对象不存在",
  invalid: "请求无效",
  conflict: "状态冲突",
  unauthorized: "未认证",
  reauth: "需要重新授权",
  server: "控制面错误",
  other: "未知错误",
};

/** Renders an unix-ms instant for operators. */
export function formatTime(ms: number): string {
  return new Date(ms).toLocaleString();
}

/** One-line operator text for a failed call or unexpected throw. */
export function describeError(error: unknown): string {
  if (error instanceof ApiError) {
    const label = KIND_LABELS[error.kind];
    const detail = error.problem.detail ?? error.problem.title;
    return `${label}：${detail}`;
  }
  if (error instanceof Error) return error.message;
  return String(error);
}

export interface StateView {
  tone: Tone;
  observation?: string;
}

const ACCOUNT_TONES: Record<string, Tone> = {
  ONLINE: "healthy",
  OFFLINE: "warn",
  UNKNOWN: "observe",
  DISABLED: "dead",
};

const SESSION_TONES: Record<string, Tone> = {
  PENDING: "busy",
  AWAITING_CONFIRMATION: "observe",
  AUTHORIZED: "healthy",
  TAKEN_OVER: "busy",
  EXPIRED: "dead",
  REVOKED: "dead",
  FAILED: "dead",
};

const OPERATION_TONES: Record<string, Tone> = {
  PENDING: "busy",
  RUNNING: "busy",
  SUCCEEDED: "done",
  FAILED: "done",
  CANCELLED: "done",
};

const UNKNOWN_ACCOUNT_NOTE = "账户状态无法观测，这不代表健康与否";

function stateView(state: string, tones: Record<string, Tone>, note?: string): StateView {
  const tone = tones[state];
  if (tone === undefined) {
    throw new Error(`unknown state name: ${state}`);
  }
  return tone === "observe" ? { tone, observation: note ?? `${state} 仅是观测结果` } : { tone };
}

/** Tone for an AccountState token; throws on names outside the machine. */
export function accountStateView(state: string): StateView {
  return stateView(state, ACCOUNT_TONES, UNKNOWN_ACCOUNT_NOTE);
}

/** Tone for an AuthSessionState token; throws on unknown names. */
export function sessionStateView(state: string): StateView {
  return stateView(state, SESSION_TONES);
}

/** Tone for an OperationState token; terminals are done, not alive. */
export function operationStateView(state: string): StateView {
  return stateView(state, OPERATION_TONES);
}

// Target states per current state, straight from the domain machines.
const ACCOUNT_ACTIONS: Record<string, string[]> = {
  ONLINE: ["OFFLINE", "UNKNOWN", "DISABLED"],
  OFFLINE: ["ONLINE", "UNKNOWN", "DISABLED"],
  UNKNOWN: ["ONLINE", "OFFLINE", "DISABLED"],
  DISABLED: [],
};

const SESSION_ACTIONS: Record<string, string[]> = {
  PENDING: ["confirm", "authorize", "expire", "revoke", "fail"],
  AWAITING_CONFIRMATION: ["authorize", "expire", "revoke", "fail"],
  AUTHORIZED: ["take-over", "expire", "revoke", "fail"],
  TAKEN_OVER: ["resume", "expire", "revoke"],
  EXPIRED: [],
  REVOKED: [],
  FAILED: [],
};

/** Account target states offered for a given current state. */
export function accountActions(state: string): string[] {
  const actions = ACCOUNT_ACTIONS[state];
  if (actions === undefined) {
    throw new Error(`unknown account state: ${state}`);
  }
  return [...actions];
}

/** REST transition verbs offered for a given session state. */
export function sessionActions(state: string): string[] {
  const actions = SESSION_ACTIONS[state];
  if (actions === undefined) {
    throw new Error(`unknown session state: ${state}`);
  }
  return [...actions];
}

export interface PermissionView {
  can: {
    readSessions: boolean;
    writeSessions: boolean;
    takeover: boolean;
  };
  /** Session scopes the key lacks, in canonical order. */
  missing: string[];
}

const SESSION_SCOPES = ["sessions:read", "sessions:write", "sessions:takeover"] as const;

/** Session capabilities of a key: what it holds and what it lacks. */
export function permissionView(scopes: readonly string[]): PermissionView {
  const held = new Set(scopes);
  const missing = SESSION_SCOPES.filter((scope) => !held.has(scope));
  return {
    can: {
      readSessions: held.has("sessions:read"),
      writeSessions: held.has("sessions:write"),
      takeover: held.has("sessions:takeover"),
    },
    missing,
  };
}
