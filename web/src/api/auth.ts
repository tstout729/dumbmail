import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { api } from "./client";
import { keys } from "./keys";
import type { Session } from "./types";

export type SignupInput = { email: string; password: string; name?: string; company: string };
export type LoginInput = { email: string; password: string };

export const authApi = {
  me: () => api<Session>("GET", "/api/me"),
  signup: (input: SignupInput) => api<Session>("POST", "/api/auth/signup", input),
  login: (input: LoginInput) => api<Session>("POST", "/api/auth/login", input),
  logout: () => api<{ ok: true }>("POST", "/api/auth/logout"),
};

export function useMe() {
  return useQuery({ queryKey: keys.me, queryFn: authApi.me, staleTime: 5 * 60_000, retry: false });
}

/** The signed-in session. Only call below the auth guard, where `me` is loaded. */
export function useSession(): Session {
  const { data } = useMe();
  if (!data) throw new Error("useSession called outside the auth guard");
  return data;
}

/** Successful signup/login mutations carry this key so the public-route guard lets the page redirect itself. */
export const authMutationKey = ["auth"] as const;

export function useSignup() {
  const qc = useQueryClient();
  return useMutation({
    mutationKey: [...authMutationKey, "signup"],
    mutationFn: authApi.signup,
    meta: { silentError: true },
    onSuccess: (session) => qc.setQueryData(keys.me, session),
  });
}

export function useLogin() {
  const qc = useQueryClient();
  return useMutation({
    mutationKey: [...authMutationKey, "login"],
    mutationFn: authApi.login,
    meta: { silentError: true },
    onSuccess: (session) => qc.setQueryData(keys.me, session),
  });
}

export function useLogout() {
  return useMutation({
    mutationFn: authApi.logout,
    // A full reload drops every cached query and any in-memory state from the old session.
    onSettled: () => window.location.assign("/login"),
  });
}
