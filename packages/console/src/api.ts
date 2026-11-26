// F035: the console's REST client over the F025/F034 control-plane
// face. Every non-2xx becomes a classified ApiError carrying the
// RFC 9457 problem document, so pages render permission and state
// failures as data instead of strings. The takeover verify call is
// the possession path: it sends the token header and never a bearer.

export interface ProblemDoc {
  type: string;
  title: string;
  status: number;
  detail?: string;
}

export type ProblemKind =
  | "forbidden"
  | "missing"
  | "invalid"
  | "conflict"
  | "unauthorized"
  | "reauth"
  | "server"
  | "other";

const PROBLEM_CLASSES: Record<string, ProblemKind> = {
  forbidden: "forbidden",
  "not-found": "missing",
  invalid: "invalid",
  conflict: "conflict",
  unauthorized: "unauthorized",
  "reauth-required": "reauth",
  "store-refused": "server",
};

/** Maps a problem type URI to the console's failure classes. */
export function classifyProblem(typeUri: string): ProblemKind {
  const tail = typeUri.split("/").pop() ?? "";
  // Only trust the class when it comes from the control plane itself.
  if (!typeUri.startsWith("https://fishpond.dev/problems/")) return "other";
  return PROBLEM_CLASSES[tail] ?? "other";
}

/** One failed call: the parsed problem document plus its class. */
export class ApiError extends Error {
  readonly problem: ProblemDoc;
  readonly kind: ProblemKind;

  constructor(problem: ProblemDoc, kind: ProblemKind) {
    super(problem.detail ?? problem.title);
    this.name = "ApiError";
    this.problem = problem;
    this.kind = kind;
  }
}

export interface AccountDto {
  id: string;
  platform: string;
  state: string;
  note?: string;
  created_at: number;
  updated_at: number;
}

export interface SessionDto {
  id: string;
  account_id: string;
  platform: string;
  state: string;
  state_note?: string;
  credential_id?: string;
  created_at: number;
  updated_at: number;
  expires_at?: number;
}

export interface OperationDto {
  id: string;
  kind: string;
  state: string;
  attempt_no?: number;
  created_at: number;
  updated_at: number;
}

export interface EventDto {
  seq: number;
  event_type: string;
  occurred_at: number;
}

export interface GrantDto {
  id: string;
  operator: string;
  token_hint: string;
  created_at: number;
  expires_at: number;
  revoked_at?: number;
}

export interface GrantIssued {
  grant_id: string;
  token: string;
  expires_at: number;
}

export interface TakeoverContext {
  grant_id: string;
  tenant: string;
  account: string;
  session: string;
  operator: string;
  expires_at: number;
}

export interface RequestOptions {
  body?: unknown;
  token?: string;
}

export class ConsoleApi {
  private readonly baseUrl: string;
  private readonly key: () => string | undefined;

  constructor(baseUrl: string, key: () => string | undefined) {
    this.baseUrl = baseUrl.replace(/\/+$/, "");
    this.key = key;
  }

  /** One call: bearer injection, JSON bodies, problem classification. */
  private async request<T>(method: string, path: string, options: RequestOptions = {}): Promise<T> {
    const headers: Record<string, string> = {};
    const bearer = this.key();
    if (bearer !== undefined && bearer !== "") headers.authorization = `Bearer ${bearer}`;
    if (options.token !== undefined) headers["x-takeover-token"] = options.token;
    let body: string | undefined;
    if (options.body !== undefined) {
      headers["content-type"] = "application/json";
      body = JSON.stringify(options.body);
    }
    const response = await fetch(`${this.baseUrl}${path}`, { method, headers, body });
    if (!response.ok) {
      throw await this.failure(response);
    }
    if (response.status === 204) return undefined as T;
    return (await response.json()) as T;
  }

  private async failure(response: Response): Promise<ApiError> {
    const raw = await response.text();
    let problem: ProblemDoc | undefined;
    try {
      const parsed = JSON.parse(raw) as Partial<ProblemDoc>;
      if (typeof parsed.type === "string" && typeof parsed.status === "number") {
        problem = {
          type: parsed.type,
          title: typeof parsed.title === "string" ? parsed.title : "error",
          status: parsed.status,
          detail: typeof parsed.detail === "string" ? parsed.detail : undefined,
        };
      }
    } catch {
      // Not a problem document — fall through to the generic view.
    }
    if (problem === undefined) {
      problem = {
        type: "https://fishpond.dev/problems/store-refused",
        title: "error",
        status: response.status,
        detail: `control plane answered ${response.status} with a non-problem body`,
      };
    }
    return new ApiError(problem, classifyProblem(problem.type));
  }

  // --- accounts -------------------------------------------------------

  listAccounts(cursor?: string): Promise<{ accounts: AccountDto[]; next_cursor?: string }> {
    const suffix = cursor === undefined ? "" : `?cursor=${encodeURIComponent(cursor)}`;
    return this.request("GET", `/accounts${suffix}`);
  }

  getAccount(id: string): Promise<AccountDto> {
    return this.request("GET", `/accounts/${encodeURIComponent(id)}`);
  }

  registerAccount(body: { id: string; platform: string }): Promise<AccountDto> {
    return this.request("POST", "/accounts", { body });
  }

  setAccountState(id: string, state: string): Promise<AccountDto> {
    return this.request("POST", `/accounts/${encodeURIComponent(id)}/state`, { body: { state } });
  }

  // --- sessions -------------------------------------------------------

  listSessions(cursor?: string): Promise<{ sessions: SessionDto[]; next_cursor?: string }> {
    const suffix = cursor === undefined ? "" : `?cursor=${encodeURIComponent(cursor)}`;
    return this.request("GET", `/sessions${suffix}`);
  }

  getSession(id: string): Promise<SessionDto> {
    return this.request("GET", `/sessions/${encodeURIComponent(id)}`);
  }

  startSession(body: { id: string; account_id: string; expires_at?: number }): Promise<SessionDto> {
    return this.request("POST", "/sessions", { body });
  }

  transitionSession(id: string, body: { action: string; note?: string }): Promise<SessionDto> {
    return this.request("POST", `/sessions/${encodeURIComponent(id)}/transition`, { body });
  }

  // --- takeover (F034) ------------------------------------------------

  issueTakeover(id: string, body: { operator: string; ttl_ms?: number }): Promise<GrantIssued> {
    return this.request("POST", `/sessions/${encodeURIComponent(id)}/takeover`, { body });
  }

  listTakeoverGrants(id: string): Promise<{ grants: GrantDto[] }> {
    return this.request("GET", `/sessions/${encodeURIComponent(id)}/takeover`);
  }

  releaseTakeover(id: string, grantId: string): Promise<{ released: boolean }> {
    return this.request(
      "DELETE",
      `/sessions/${encodeURIComponent(id)}/takeover/${encodeURIComponent(grantId)}`,
    );
  }

  /** Possession path: the token is the credential, no bearer key. */
  verifyTakeover(token: string): Promise<TakeoverContext> {
    return this.request("POST", "/takeover/verify", { token });
  }

  // --- operations, events, health --------------------------------------

  listOperations(cursor?: string): Promise<{ operations: OperationDto[]; next_cursor?: string }> {
    const suffix = cursor === undefined ? "" : `?cursor=${encodeURIComponent(cursor)}`;
    return this.request("GET", `/operations${suffix}`);
  }

  getOperation(id: string): Promise<OperationDto> {
    return this.request("GET", `/operations/${encodeURIComponent(id)}`);
  }

  listEvents(cursor?: string): Promise<{ events: EventDto[]; next_cursor?: string }> {
    const suffix = cursor === undefined ? "" : `?cursor=${encodeURIComponent(cursor)}`;
    return this.request("GET", `/events${suffix}`);
  }

  status(): Promise<Record<string, unknown>> {
    return this.request("GET", "/status");
  }

  health(): Promise<{ status: string; service: string; version: string }> {
    return this.request("GET", "/healthz");
  }
}
