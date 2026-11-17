import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { api, withQuery } from "../../api/client";
import { keys } from "../../api/keys";
import { useSession } from "../../api";
import type { Session } from "../../api/types";
import type {
  AccountSecurity,
  AuditFilters,
  AuditPage,
  CreatedScopedKey,
  InviteResult,
  NewKeyInput,
  PublicInvite,
  Role,
  ScopedApiKey,
  SessionRow,
  SsoInput,
  SsoSettings,
  Team,
  TeamSession,
  TeamSessionInfo,
  TwoFactorSetup,
  TwoFactorStatus,
} from "./types";

const teamKeys = {
  team: ["team"] as const,
  security: ["team", "security"] as const,
  sso: ["team", "sso"] as const,
  twoFactor: ["me", "two-factor"] as const,
  sessions: ["me", "sessions"] as const,
  audit: (f: AuditFilters) => ["audit", f] as const,
  invite: (token: string) => ["invite", token] as const,
};

export const teamApi = {
  team: () => api<Team>("GET", "/api/team"),
  invite: (input: { email: string; role: Role }) => api<InviteResult>("POST", "/api/team/invites", input),
  resendInvite: (id: string) => api<InviteResult>("POST", `/api/team/invites/${id}/resend`),
  revokeInvite: (id: string) => api<{ ok: true }>("DELETE", `/api/team/invites/${id}`),
  changeRole: (input: { userId: string; role: Role }) => api<{ role: Role }>("PATCH", `/api/team/members/${input.userId}`, { role: input.role }),
  removeMember: (userId: string) => api<{ ok: true }>("DELETE", `/api/team/members/${userId}`),
  leave: () => api<{ ok: true }>("POST", "/api/team/leave"),
  transfer: (userId: string) => api<Team>("POST", "/api/team/transfer", { user_id: userId }),
  switchAccount: (accountId: string) => api<TeamSession>("POST", "/api/accounts/switch", { account_id: accountId }),
  createAccount: (name: string) => api<TeamSession>("POST", "/api/accounts", { name }),
  verifyCode: (code: string) => api<TeamSession>("POST", "/api/auth/two-factor", { code }),
  twoFactor: () => api<TwoFactorStatus>("GET", "/api/me/two-factor"),
  setupTwoFactor: () => api<TwoFactorSetup>("POST", "/api/me/two-factor/setup"),
  enableTwoFactor: (code: string) => api<{ enabled: true; recovery_codes: string[] }>("POST", "/api/me/two-factor/enable", { code }),
  disableTwoFactor: (code: string) => api<{ enabled: false }>("POST", "/api/me/two-factor/disable", { code }),
  newRecoveryCodes: (code: string) => api<{ recovery_codes: string[] }>("POST", "/api/me/two-factor/recovery-codes", { code }),
  sessions: () => api<{ data: SessionRow[] }>("GET", "/api/me/sessions"),
  revokeSession: (id: string) => api<{ ok: true; current: boolean }>("DELETE", `/api/me/sessions/${id}`),
  revokeOthers: () => api<{ ok: true; revoked: number }>("POST", "/api/me/sessions/revoke-others"),
  changePassword: (input: { current_password: string; new_password: string }) => api<{ ok: true; other_sessions_revoked: number }>("POST", "/api/me/password", input),
  forgot: (email: string) => api<{ ok: true; message: string }>("POST", "/api/auth/password/forgot", { email }),
  reset: (input: { token: string; password: string }) => api<{ ok: true }>("POST", "/api/auth/password/reset", input),
  security: () => api<AccountSecurity>("GET", "/api/team/security"),
  updateSecurity: (requireTwoFactor: boolean) => api<AccountSecurity>("PATCH", "/api/team/security", { require_two_factor: requireTwoFactor }),
  sso: () => api<{ sso: SsoSettings | null; redirect_uri: string }>("GET", "/api/team/sso"),
  saveSso: (input: SsoInput) => api<{ sso: SsoSettings; redirect_uri: string }>("PUT", "/api/team/sso", input),
  removeSso: () => api<{ sso: null; redirect_uri: string }>("DELETE", "/api/team/sso"),
  audit: (f: AuditFilters) => api<AuditPage>("GET", withQuery("/api/audit", { ...f, limit: 50 })),
  publicInvite: (token: string) => api<PublicInvite>("GET", `/api/invites/${encodeURIComponent(token)}`),
  acceptInvite: (input: { token: string; name?: string; password?: string }) =>
    api<TeamSession>("POST", `/api/invites/${encodeURIComponent(input.token)}/accept`, { name: input.name, password: input.password }),
  keys: () => api<{ data: ScopedApiKey[]; scopes: string[] }>("GET", "/api/api-keys"),
  createKey: (input: NewKeyInput) => api<CreatedScopedKey>("POST", "/api/api-keys", input),
  revokeKey: (id: string) => api<{ ok: true }>("DELETE", `/api/api-keys/${id}`),
};

/** The team and sign-in fields of `/api/me`. Only below the auth guard. */
export function useTeamSession(): TeamSessionInfo {
  const s = useSession() as Session & Partial<TeamSessionInfo>;
  return {
    role: s.role ?? "owner",
    permissions: s.permissions ?? [],
    restriction: s.restriction ?? null,
    restriction_message: s.restriction_message ?? null,
    sign_in_method: s.sign_in_method ?? null,
    two_factor_enabled: s.two_factor_enabled ?? false,
    accounts: s.accounts ?? [],
  };
}

/** Whether the signed-in member holds a permission (the server checks it again). */
export function useCan(permission: string): boolean {
  return useTeamSession().permissions.includes(permission);
}

/** A full reload after switching accounts drops every cached query from the old one. */
function reloadHome() {
  window.location.assign("/");
}

export function useTeam() {
  return useQuery({ queryKey: teamKeys.team, queryFn: teamApi.team });
}

function useTeamMutation<V, R>(fn: (v: V) => Promise<R>, silent = false) {
  const qc = useQueryClient();
  return useMutation({ mutationFn: fn, meta: { silentError: silent }, onSettled: () => qc.invalidateQueries({ queryKey: teamKeys.team }) });
}

export const useInvite = () => useTeamMutation(teamApi.invite, true);
export const useResendInvite = () => useTeamMutation(teamApi.resendInvite);
export const useRevokeInvite = () => useTeamMutation(teamApi.revokeInvite);
export const useChangeRole = () => useTeamMutation(teamApi.changeRole);
export const useRemoveMember = () => useTeamMutation(teamApi.removeMember);

export function useTransferOwnership() {
  return useMutation({ mutationFn: teamApi.transfer, onSuccess: reloadHome });
}

export function useLeaveAccount() {
  return useMutation({ mutationFn: teamApi.leave, onSuccess: reloadHome });
}

export function useSwitchAccount() {
  return useMutation({ mutationFn: teamApi.switchAccount, onSuccess: reloadHome });
}

export function useCreateAccount() {
  return useMutation({ mutationFn: teamApi.createAccount, meta: { silentError: true }, onSuccess: () => window.location.assign("/onboarding") });
}

export function useVerifyCode() {
  const qc = useQueryClient();
  return useMutation({ mutationFn: teamApi.verifyCode, meta: { silentError: true }, onSuccess: () => qc.invalidateQueries({ queryKey: keys.me }) });
}

export function useTwoFactor() {
  return useQuery({ queryKey: teamKeys.twoFactor, queryFn: teamApi.twoFactor });
}

export function useSetupTwoFactor() {
  return useMutation({ mutationFn: teamApi.setupTwoFactor });
}

function useTwoFactorMutation<R>(fn: (code: string) => Promise<R>) {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: fn,
    meta: { silentError: true },
    onSuccess: () => {
      void qc.invalidateQueries({ queryKey: teamKeys.twoFactor });
      void qc.invalidateQueries({ queryKey: teamKeys.security });
    },
  });
}

export const useEnableTwoFactor = () => useTwoFactorMutation(teamApi.enableTwoFactor);
export const useDisableTwoFactor = () => useTwoFactorMutation(teamApi.disableTwoFactor);
export const useNewRecoveryCodes = () => useTwoFactorMutation(teamApi.newRecoveryCodes);

export function useSessions() {
  return useQuery({ queryKey: teamKeys.sessions, queryFn: teamApi.sessions, select: (r) => r.data });
}

export function useRevokeSession() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: teamApi.revokeSession,
    onSuccess: (r) => {
      if (r.current) window.location.assign("/login");
    },
    onSettled: () => qc.invalidateQueries({ queryKey: teamKeys.sessions }),
  });
}

export function useRevokeOtherSessions() {
  const qc = useQueryClient();
  return useMutation({ mutationFn: teamApi.revokeOthers, onSettled: () => qc.invalidateQueries({ queryKey: teamKeys.sessions }) });
}

export function useChangePassword() {
  const qc = useQueryClient();
  return useMutation({ mutationFn: teamApi.changePassword, meta: { silentError: true }, onSuccess: () => qc.invalidateQueries({ queryKey: teamKeys.sessions }) });
}

export function useForgotPassword() {
  return useMutation({ mutationFn: teamApi.forgot, meta: { silentError: true } });
}

export function useResetPassword() {
  return useMutation({ mutationFn: teamApi.reset, meta: { silentError: true } });
}

export function useAccountSecurity(enabled = true) {
  return useQuery({ queryKey: teamKeys.security, queryFn: teamApi.security, enabled });
}

export function useUpdateSecurity() {
  const qc = useQueryClient();
  return useMutation({ mutationFn: teamApi.updateSecurity, onSuccess: (d) => qc.setQueryData(teamKeys.security, d) });
}

export function useSso(enabled = true) {
  return useQuery({ queryKey: teamKeys.sso, queryFn: teamApi.sso, enabled });
}

export function useSaveSso() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: teamApi.saveSso,
    meta: { silentError: true },
    onSuccess: (d) => {
      qc.setQueryData(teamKeys.sso, d);
      void qc.invalidateQueries({ queryKey: teamKeys.security });
    },
  });
}

export function useRemoveSso() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: teamApi.removeSso,
    onSuccess: (d) => {
      qc.setQueryData(teamKeys.sso, d);
      void qc.invalidateQueries({ queryKey: teamKeys.security });
    },
  });
}

export function useAudit(filters: AuditFilters, enabled = true) {
  return useQuery({ queryKey: teamKeys.audit(filters), queryFn: () => teamApi.audit(filters), enabled, placeholderData: (prev) => prev });
}

export function auditExportHref(filters: AuditFilters): string {
  return withQuery("/api/audit/export", { action: filters.action, q: filters.q, from: filters.from, to: filters.to });
}

export function usePublicInvite(token: string) {
  return useQuery({ queryKey: teamKeys.invite(token), queryFn: () => teamApi.publicInvite(token), retry: false });
}

export function useAcceptInvite() {
  return useMutation({ mutationFn: teamApi.acceptInvite, meta: { silentError: true }, onSuccess: reloadHome });
}

export function useScopedKeys(enabled = true) {
  return useQuery({ queryKey: keys.apiKeys, queryFn: teamApi.keys, enabled });
}

export function useCreateScopedKey() {
  const qc = useQueryClient();
  return useMutation({ mutationFn: teamApi.createKey, meta: { silentError: true }, onSuccess: () => qc.invalidateQueries({ queryKey: keys.apiKeys }) });
}

export function useRevokeScopedKey() {
  const qc = useQueryClient();
  return useMutation({ mutationFn: teamApi.revokeKey, onSettled: () => qc.invalidateQueries({ queryKey: keys.apiKeys }) });
}
