// The one place `fetch` is called.
//
// Everything above this file works with typed values and typed failures; no
// component builds a URL or parses a body.

import type { ApiErrorBody } from "./types";

/** Every request goes to the same origin that served this page. */
const BASE = "/api/v1";

/**
 * A failure the server described.
 *
 * `code` is the interface: it is stable, and a caller switches on it. `message`
 * is for a human and may be reworded at any time.
 */
export class ApiError extends Error {
  readonly status: number;
  readonly code: string;
  readonly causes: string[];
  readonly activeOperationId?: string;
  readonly activeOperationKind?: string;

  constructor(status: number, body: Partial<ApiErrorBody>) {
    super(body.message ?? `request failed with status ${status}`);
    this.name = "ApiError";
    this.status = status;
    this.code = body.code ?? "unknown";
    this.causes = body.causes ?? [];
    this.activeOperationId = body.active_operation_id;
    this.activeOperationKind = body.active_operation_kind;
  }

  /** Whether another operation holds the workspace. */
  get isWorkspaceBusy(): boolean {
    return this.code === "workspace_busy";
  }

  /**
   * Whether the server does not know this operation.
   *
   * This is not a failure of the work: it means the identity was never
   * allocated, has been evicted, or belonged to a process that has restarted.
   * A caller should stop tracking it and read durable state instead.
   */
  get isUnknownOperation(): boolean {
    return this.code === "operation_not_found";
  }
}

async function request<T>(
  method: "GET" | "POST",
  path: string,
  signal?: AbortSignal,
): Promise<T> {
  const response = await fetch(`${BASE}${path}`, {
    method,
    headers: { Accept: "application/json", "X-Mineral-Request": "1" },
    signal,
  });

  if (!response.ok) {
    if (response.status === 401) window.dispatchEvent(new Event("mineral-auth-expired"));
    // A failure the API described is JSON. Anything else (a proxy error page, a
    // dropped connection) becomes a synthetic error with the real status.
    let body: Partial<ApiErrorBody> = {};
    try {
      body = (await response.json()) as Partial<ApiErrorBody>;
    } catch {
      body = { code: "transport_error", message: response.statusText };
    }
    throw new ApiError(response.status, body);
  }

  // 202 carries a body; every success here does.
  return (await response.json()) as T;
}

export const api = {
  get: <T>(path: string, signal?: AbortSignal): Promise<T> =>
    request<T>("GET", path, signal),
  post: <T>(path: string, signal?: AbortSignal): Promise<T> =>
    request<T>("POST", path, signal),
};

/** The URL an `EventSource` should open for one operation. */
export function eventsUrl(operationId: string): string {
  return `${BASE}/operations/${encodeURIComponent(operationId)}/events`;
}
