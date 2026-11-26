// GENERATED FILE — scripts/generate_sdk.py from crates/server/tests/fixtures/openapi.json. Do not edit by hand.

/** One RFC 9457 problem document. */
export interface Problem {
  type: string;
  title: string;
  status: number;
  detail?: string;
}

export interface Account {
  id: string;
  platform: string;
  state: "ONLINE" | "OFFLINE" | "UNKNOWN" | "DISABLED";
  state_note?: string;
  /** unix ms */
  updated_at: number;
}
export interface AccountList {
  accounts: Account[];
}
export interface AuthSession {
  account_id: string;
  /** unix ms */
  created_at: number;
  credential_id?: string;
  /** unix ms */
  expires_at?: number;
  id: string;
  platform: string;
  state: "PENDING" | "AWAITING_CONFIRMATION" | "AUTHORIZED" | "EXPIRED" | "REVOKED" | "FAILED" | "TAKEN_OVER";
  state_note?: string;
  /** unix ms */
  updated_at: number;
}
export interface ClaimOperation {
  /** client-chosen, unique per attempt */
  attempt_id: string;
  /** attempt deadline, unix ms */
  deadline_at: number;
  worker_id: string;
}
export interface ClaimedOperation {
  attempt: { attempt_id: string; attempt_no: number; deadline_at: number };
  operation: Operation;
}
export interface CloseAttempt {
  result: "succeeded" | "failed" | "timed-out";
  worker_id: string;
}
export interface CreateOperation {
  /** client-chosen, unique per tenant */
  id: string;
  /** what to do (e.g. send_message) */
  kind: string;
}
export interface CreatedOperation {
  operation: Operation;
  /** true when the key returned the original, not new work */
  replayed: boolean;
}
export interface Event {
  account_id?: string;
  event_type: string;
  id: string;
  /** unix ms */
  occurred_at: number;
  /** event-specific JSON payload */
  payload?: unknown;
  /** log position */
  seq: number;
}
export interface EventPage {
  events: Event[];
  next_cursor: string | null;
}
export interface Grant {
  /** unix ms */
  created_at: number;
  /** unix ms */
  expires_at: number;
  id: string;
  operator: string;
  /** unix ms, when revoked */
  revoked_at?: number;
  /** display tail of the hashed token */
  token_hint: string;
}
export interface GrantIssued {
  expires_at: number;
  grant_id: string;
  /** Raw token; store it now — the server only holds a hash */
  token: string;
}
export interface GrantList {
  grants: Grant[];
}
export interface Health {
  service: string;
  status: string;
  version: string;
}
export interface Operation {
  /** unix ms */
  created_at: number;
  id: string;
  kind: string;
  state: "PENDING" | "RUNNING" | "SUCCEEDED" | "FAILED" | "CANCELLED";
}
export interface OperationPage {
  next_cursor: string | null;
  operations: Operation[];
}
export interface RegisterAccount {
  id: string;
  platform: string;
}
export interface ReleaseResult {
  released: boolean;
}
export interface SessionPage {
  next_cursor: string | null;
  sessions: AuthSession[];
}
export interface SetAccountState {
  note?: string;
  state: string;
}
export interface StartSession {
  account_id: string;
  /** unix ms */
  expires_at?: number;
  /** client-chosen, unique per tenant */
  id: string;
}
export interface StartTakeover {
  /** Who the credential is for (audit display) */
  operator: string;
  /** Requested life in ms; defaults to the service cap */
  ttl_ms?: number;
}
export interface StatusSummary {
  disabled: number;
  offline: number;
  online: number;
  total: number;
  unknown: number;
}
export interface TakeoverContext {
  account: string;
  expires_at: number;
  grant_id: string;
  operator: string;
  session: string;
  tenant: string;
}
export interface TransitionSession {
  action: "confirm" | "authorize" | "expire" | "revoke" | "fail" | "take-over" | "resume";
  /** authorize only */
  credential_id?: string;
  /** authorize only */
  credential_version?: number;
  /** authorize only */
  expires_at?: number;
  note?: string;
}

export type ProblemKind =
  | "conflict"
  | "forbidden"
  | "invalid"
  | "missing"
  | "other"
  | "reauth"
  | "server"
  | "unauthorized"
;

const PROBLEM_KINDS: Record<string, ProblemKind> = {
  "conflict": "conflict",
  "forbidden": "forbidden",
  "invalid": "invalid",
  "invalid-cursor": "invalid",
  "not-found": "missing",
  "reauth-required": "reauth",
  "scope-denied": "forbidden",
  "storage": "server",
  "store-refused": "server",
  "unauthorized": "unauthorized",
};

/** Maps a problem type URI to the SDK's failure classes. */
export function classifyProblem(typeUri: string): ProblemKind {
  if (!typeUri.startsWith("https://fishpond.dev/problems/")) return "other";
  return PROBLEM_KINDS[typeUri.split("/").pop() ?? ""] ?? "other";
}

/** One failed call: the parsed problem plus its class. */
export class ApiError extends Error {
  readonly problem: Problem;
  readonly kind: ProblemKind;

  constructor(problem: Problem, kind: ProblemKind) {
    super(problem.detail ?? problem.title);
    this.name = "ApiError";
    this.problem = problem;
    this.kind = kind;
  }
}

interface RequestOptions {
  bearer?: boolean;
  headers?: Record<string, string>;
  body?: unknown;
  raw?: boolean;
}

/** Typed client for the fishpond control plane. */
export class FishpondClient {
  private readonly baseUrl: string;
  private readonly key: (() => string | undefined) | undefined;

  constructor(baseUrl: string, key?: () => string | undefined) {
    this.baseUrl = baseUrl.replace(/\/+$/, "");
    this.key = key;
  }

  private async request<T>(
    method: string,
    path: string,
    options: RequestOptions = {},
  ): Promise<T> {
    const headers: Record<string, string> = { ...(options.headers ?? {}) };
    const key = this.key?.();
    if (options.bearer !== false && key !== undefined && key !== "") {
      headers.authorization = `Bearer ${key}`;
    }
    if (options.raw) headers.accept = "text/event-stream";
    let bodyText: string | undefined;
    if (options.body !== undefined) {
      headers["content-type"] = "application/json";
      bodyText = JSON.stringify(options.body);
    }
    const response = await fetch(`${this.baseUrl}${path}`, {
      method,
      headers,
      body: bodyText,
    });
    if (!response.ok) {
      const text = await response.text();
      let problem: Problem;
      try {
        problem = JSON.parse(text) as Problem;
      } catch {
        problem = {
          type: "https://fishpond.dev/problems/store-refused",
          title: "error",
          status: response.status,
          detail: `control plane answered ${response.status} with a non-problem body`,
        };
      }
      throw new ApiError(problem, classifyProblem(problem.type));
    }
    if (options.raw) return (await response.text()) as unknown as T;
    if (response.status === 204) return undefined as T;
    return (await response.json()) as T;
  }

  /** Account counts by state */
  async accountStatus(): Promise<StatusSummary> {
    const path = `/status`;
    return this.request<StatusSummary>("GET", path, {
    });
  }

  /** Claim a PENDING operation (CAS to RUNNING with a numbered attempt) */
  async claimOperation(id: string, body: ClaimOperation): Promise<ClaimedOperation> {
    const path = `/operations/${encodeURIComponent(id)}/claim`;
    return this.request<ClaimedOperation>("POST", path, {
      body,
    });
  }

  /** Settle one attempt (succeeded/failed end the operation; timed-out requeues) */
  async closeAttempt(id: string, attemptId: string, body: CloseAttempt): Promise<Operation> {
    const path = `/operations/${encodeURIComponent(id)}/attempts/${encodeURIComponent(attemptId)}/close`;
    return this.request<Operation>("POST", path, {
      body,
    });
  }

  /** Idempotently create an operation (PENDING) */
  async createOperation(idempotencyKey: string, body: CreateOperation): Promise<CreatedOperation> {
    const path = `/operations`;
    const headers: Record<string, string> = { "Idempotency-Key": idempotencyKey };
    return this.request<CreatedOperation>("POST", path, {
      headers,
      body,
    });
  }

  /** One account */
  async getAccount(id: string): Promise<Account> {
    const path = `/accounts/${encodeURIComponent(id)}`;
    return this.request<Account>("GET", path, {
    });
  }

  /** One operation */
  async getOperation(id: string): Promise<Operation> {
    const path = `/operations/${encodeURIComponent(id)}`;
    return this.request<Operation>("GET", path, {
    });
  }

  /** One auth session */
  async getSession(id: string): Promise<AuthSession> {
    const path = `/sessions/${encodeURIComponent(id)}`;
    return this.request<AuthSession>("GET", path, {
    });
  }

  /** Liveness probe */
  async health(): Promise<Health> {
    const path = `/healthz`;
    return this.request<Health>("GET", path, {
      bearer: false,
    });
  }

  /** List the tenant's accounts (ordered by id) */
  async listAccounts(): Promise<AccountList> {
    const path = `/accounts`;
    return this.request<AccountList>("GET", path, {
    });
  }

  /** Bounded page of the tenant's event log (cursor pagination) */
  async listEvents(cursor?: string, limit?: number): Promise<EventPage> {
    const path = `/events`;
    const query = new URLSearchParams();
    if (cursor !== undefined) query.set("cursor", String(cursor));
    if (limit !== undefined) query.set("limit", String(limit));
    const qs = query.toString();
    return this.request<EventPage>("GET", qs === "" ? path : `${path}?${qs}`, {
    });
  }

  /** List the tenant's operations (oldest first, cursor pagination) */
  async listOperations(cursor?: string, limit?: number): Promise<OperationPage> {
    const path = `/operations`;
    const query = new URLSearchParams();
    if (cursor !== undefined) query.set("cursor", String(cursor));
    if (limit !== undefined) query.set("limit", String(limit));
    const qs = query.toString();
    return this.request<OperationPage>("GET", qs === "" ? path : `${path}?${qs}`, {
    });
  }

  /** Cursor page of the tenant's auth sessions (ordered by created_at, id) */
  async listSessions(cursor?: string, limit?: number): Promise<SessionPage> {
    const path = `/sessions`;
    const query = new URLSearchParams();
    if (cursor !== undefined) query.set("cursor", String(cursor));
    if (limit !== undefined) query.set("limit", String(limit));
    const qs = query.toString();
    return this.request<SessionPage>("GET", qs === "" ? path : `${path}?${qs}`, {
    });
  }

  /** The session's takeover grant audit list (hashes and hints, never raw tokens) */
  async listTakeoverGrants(id: string): Promise<GrantList> {
    const path = `/sessions/${encodeURIComponent(id)}/takeover`;
    return this.request<GrantList>("GET", path, {
    });
  }

  /** Register an account (OFFLINE) */
  async registerAccount(body: RegisterAccount): Promise<Account> {
    const path = `/accounts`;
    return this.request<Account>("POST", path, {
      body,
    });
  }

  /** Release the takeover: the credential dies and automation resumes */
  async releaseTakeover(id: string, grantId: string): Promise<ReleaseResult> {
    const path = `/sessions/${encodeURIComponent(id)}/takeover/${encodeURIComponent(grantId)}`;
    return this.request<ReleaseResult>("DELETE", path, {
    });
  }

  /** One account state-machine step */
  async setAccountState(id: string, body: SetAccountState): Promise<Account> {
    const path = `/accounts/${encodeURIComponent(id)}/state`;
    return this.request<Account>("POST", path, {
      body,
    });
  }

  /** Start a login session (PENDING) */
  async startSession(body: StartSession): Promise<AuthSession> {
    const path = `/sessions`;
    return this.request<AuthSession>("POST", path, {
      body,
    });
  }

  /** Take the session over and mint the operator credential (the raw token appears exactly once) */
  async startTakeover(id: string, body: StartTakeover): Promise<GrantIssued> {
    const path = `/sessions/${encodeURIComponent(id)}/takeover`;
    return this.request<GrantIssued>("POST", path, {
      body,
    });
  }

  /** SSE stream of the tenant's event log */
  async streamEvents(consumer?: string, lastEventID?: string): Promise<string> {
    const path = `/events/stream`;
    const headers: Record<string, string> = {};
    if (lastEventID !== undefined) headers["Last-Event-ID"] = lastEventID;
    const query = new URLSearchParams();
    if (consumer !== undefined) query.set("consumer", String(consumer));
    const qs = query.toString();
    return this.request<string>("GET", qs === "" ? path : `${path}?${qs}`, {
      headers,
      raw: true,
    });
  }

  /** One named state step (confirm/authorize/expire/revoke/fail/take-over/resume) */
  async transitionSession(id: string, body: TransitionSession): Promise<AuthSession> {
    const path = `/sessions/${encodeURIComponent(id)}/transition`;
    return this.request<AuthSession>("POST", path, {
      body,
    });
  }

  /** Verify an operator takeover token by possession (X-Takeover-Token header; no bearer key) */
  async verifyTakeover(xTakeoverToken: string): Promise<TakeoverContext> {
    const path = `/takeover/verify`;
    const headers: Record<string, string> = { "X-Takeover-Token": xTakeoverToken };
    return this.request<TakeoverContext>("POST", path, {
      bearer: false,
      headers,
    });
  }
}

