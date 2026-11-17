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

let unauthorizedHandler: (() => void) | null = null;

/** Called once by the app shell so any 401 (session expired) sends the user to /login. */
export function onUnauthorized(handler: () => void): () => void {
  unauthorizedHandler = handler;
  return () => {
    if (unauthorizedHandler === handler) unauthorizedHandler = null;
  };
}

export async function api<T>(method: Method, path: string, body?: unknown): Promise<T> {
  let res: Response;
  try {
    res = await fetch(path, {
      method,
      credentials: "include",
      headers: body === undefined ? { Accept: "application/json" } : { "Content-Type": "application/json", Accept: "application/json" },
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
    throw new ApiError(res.status, code, message);
  }

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
