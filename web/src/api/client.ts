import { MutationCache, QueryClient } from "@tanstack/react-query";
import { toast } from "../lib/toast";
import type { ApiErrorCode } from "./types";

export class ApiError extends Error {
  readonly status: number;
  readonly code: ApiErrorCode | "network";

  constructor(status: number, code: ApiErrorCode | "network", message: string) {
    super(message);
    this.name = "ApiError";
    this.status = status;
    this.code = code;
  }
}

type Method = "GET" | "POST" | "PUT" | "PATCH" | "DELETE";

type Query = Record<string, string | number | boolean | null | undefined>;

export function withQuery(path: string, query?: Query): string {
  if (!query) return path;
  const params = new URLSearchParams();
  for (const [key, value] of Object.entries(query)) {
    if (value === undefined || value === null || value === "") continue;
    params.set(key, String(value));
  }
  const qs = params.toString();
  return qs ? `${path}?${qs}` : path;
}

/**
 * The account this page was loaded for. Every write says which one (`X-DumbMail-Account`), so a
 * tab still showing another account (after a switch in another tab) can't save into the one the
 * session has open now: the server answers 409 `account_changed` and the page reloads.
 */
let boundAccount: string | null = null;

/** Remember the account the page shows; if the session's account changed underneath, reload. */
function bindAccount(json: unknown) {
  const session = json as { account?: { id?: unknown }; permissions?: unknown } | null;
  const id = session?.account?.id;
  if (typeof id !== "string" || !Array.isArray(session?.permissions)) return;
  if (boundAccount === null) boundAccount = id;
  else if (boundAccount !== id) window.location.reload();
}

/** Responses that open a different account (as does accepting an invite); the page reloads right after them. */
const SWITCHES = ["/api/accounts", "/api/accounts/switch"];

const ACCOUNT_CHANNEL = "dumbmail-account";

/** Tell other tabs the open account changed, so they reload instead of saving into it. */
export function announceAccountSwitch() {
  if (typeof BroadcastChannel === "undefined") return;
  const ch = new BroadcastChannel(ACCOUNT_CHANNEL);
  ch.postMessage("switched");
  ch.close();
}

if (typeof BroadcastChannel !== "undefined") {
  new BroadcastChannel(ACCOUNT_CHANNEL).onmessage = () => window.location.reload();
}

let unauthorizedHandler: (() => void) | null = null;

/** Called once by the app shell so any 401 (session expired) sends the user to /login. */
export function onUnauthorized(handler: () => void): () => void {
  unauthorizedHandler = handler;
  return () => {
    if (unauthorizedHandler === handler) unauthorizedHandler = null;
  };
}

/**
 * Headers every write adds: the account this page shows. The server refuses writes made with the
 * session cookie that don't say it. For requests that can't go through `api` (uploads, downloads).
 */
export function writeHeaders(): Record<string, string> {
  return boundAccount ? { "X-DumbMail-Account": boundAccount } : {};
}

/** The server said this page shows another account (or is out of date): reload it. */
export function reloadIfAccountChanged(status: number, code: string | undefined) {
  if (status === 409 && code === "account_changed") window.location.reload();
}

export async function api<T>(method: Method, path: string, body?: unknown): Promise<T> {
  let res: Response;
  const headers: Record<string, string> = { Accept: "application/json" };
  if (body !== undefined) headers["Content-Type"] = "application/json";
  if (method !== "GET") Object.assign(headers, writeHeaders());
  try {
    res = await fetch(path, {
      method,
      credentials: "include",
      headers,
      body: body === undefined ? undefined : JSON.stringify(body),
    });
  } catch {
    throw new ApiError(0, "network", "Can't reach the DumbMail server. Check your connection and try again.");
  }

  const text = await res.text();
  let json: unknown = null;
  if (text) {
    try {
      json = JSON.parse(text);
    } catch {
      json = null;
    }
  }

  if (!res.ok) {
    const err = (json as { error?: { code?: ApiErrorCode; message?: string } } | null)?.error;
    const code = err?.code ?? (res.status === 401 ? "unauthorized" : res.status === 404 ? "not_found" : "internal");
    const message = err?.message ?? `Request failed (${res.status})`;
    if (res.status === 401 && !path.startsWith("/api/auth/") && path !== "/api/me") unauthorizedHandler?.();
    reloadIfAccountChanged(res.status, code);
    throw new ApiError(res.status, code, message);
  }

  if (!SWITCHES.includes(path) && !path.startsWith("/api/invites/")) bindAccount(json);
  return json as T;
}

export function errorMessage(error: unknown): string {
  if (error instanceof ApiError) return error.message;
  if (error instanceof Error) return error.message;
  return "Something went wrong";
}

declare module "@tanstack/react-query" {
  interface Register {
    // Set `meta: { silentError: true }` on a mutation that shows its own inline error.
    mutationMeta: { silentError?: boolean };
  }
}

export const queryClient = new QueryClient({
  mutationCache: new MutationCache({
    onError: (error, _vars, _ctx, mutation) => {
      if (!mutation.meta?.silentError) toast.error(errorMessage(error));
    },
  }),
  defaultOptions: {
    queries: {
      staleTime: 30_000,
      refetchOnWindowFocus: false,
      retry: (count, error) => {
        if (error instanceof ApiError && error.status >= 400 && error.status < 500) return false;
        return count < 2;
      },
    },
  },
});
