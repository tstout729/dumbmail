import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { api } from "./client";
import { keys } from "./keys";
import type { Account, AccountPatch, ApiKey, BrandKit, CreatedApiKey, Sending, Session } from "./types";

export const accountApi = {
  update: (patch: AccountPatch) => api<Account>("PATCH", "/api/account", patch),
  importBrand: (url: string) => api<{ brand: BrandKit }>("POST", "/api/brand/import", { url }),
  sending: () => api<Sending>("GET", "/api/account/sending"),
  addDomain: (domain: string) => api<Sending>("POST", "/api/account/domains", { domain }),
  verifyDomain: (domain: string) => api<Sending>("POST", `/api/account/domains/${encodeURIComponent(domain)}/verify`),
  removeDomain: (domain: string) => api<Sending>("DELETE", `/api/account/domains/${encodeURIComponent(domain)}`),
  setLinkDomain: (domain: string) => api<Sending>("POST", "/api/account/link-domain", { domain }),
  verifyLinkDomain: () => api<Sending>("POST", "/api/account/link-domain/verify"),
  removeLinkDomain: () => api<Sending>("DELETE", "/api/account/link-domain"),
  apiKeys: () => api<{ data: ApiKey[] }>("GET", "/api/api-keys"),
  createApiKey: (name: string) => api<CreatedApiKey>("POST", "/api/api-keys", { name }),
  revokeApiKey: (id: string) => api<{ ok: true }>("DELETE", `/api/api-keys/${id}`),
};

export function useUpdateAccount() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: accountApi.update,
    onMutate: async (patch) => {
      await qc.cancelQueries({ queryKey: keys.me });
      const previous = qc.getQueryData<Session>(keys.me);
      if (previous) {
        const { onboarding, ...rest } = patch;
        qc.setQueryData<Session>(keys.me, {
          ...previous,
          account: {
            ...previous.account,
            ...rest,
            onboarding: { ...previous.account.onboarding, ...onboarding },
          },
        });
      }
      return { previous };
    },
    onError: (_err, _patch, ctx) => {
      if (ctx?.previous) qc.setQueryData(keys.me, ctx.previous);
    },
    onSuccess: (account) => {
      qc.setQueryData<Session>(keys.me, (old) => (old ? { ...old, account } : old));
    },
  });
}

export function useSending() {
  return useQuery({ queryKey: keys.sending, queryFn: accountApi.sending });
}

/** Every domain endpoint returns the full `Sending` shape, so each mutation just replaces the cache. */
function useSendingMutation<V>(mutationFn: (vars: V) => Promise<Sending>) {
  const qc = useQueryClient();
  return useMutation({ mutationFn, onSuccess: (data) => qc.setQueryData(keys.sending, data) });
}

export const useAddDomain = () => useSendingMutation<string>(accountApi.addDomain);
export const useVerifyDomain = () => useSendingMutation<string>(accountApi.verifyDomain);
export const useRemoveDomain = () => useSendingMutation<string>(accountApi.removeDomain);

/** Whether `email` is on one of the account's verified sending domains (or a subdomain of one). */
export function isVerifiedSender(sending: Sending | undefined, email: string | null | undefined): boolean {
  const host = email?.split("@")[1]?.toLowerCase();
  if (!sending || !host) return false;
  return sending.email.domains.some((d) => d.status === "verified" && (host === d.domain || host.endsWith(`.${d.domain}`)));
}
export const useSetLinkDomain = () => useSendingMutation<string>(accountApi.setLinkDomain);
export const useVerifyLinkDomain = () => useSendingMutation<void>(accountApi.verifyLinkDomain);
export const useRemoveLinkDomain = () => useSendingMutation<void>(accountApi.removeLinkDomain);

export function useApiKeys() {
  return useQuery({ queryKey: keys.apiKeys, queryFn: accountApi.apiKeys, select: (r) => r.data });
}

export function useCreateApiKey() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: accountApi.createApiKey,
    onSuccess: () => qc.invalidateQueries({ queryKey: keys.apiKeys }),
  });
}

export function useRevokeApiKey() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: accountApi.revokeApiKey,
    onMutate: async (id) => {
      await qc.cancelQueries({ queryKey: keys.apiKeys });
      const previous = qc.getQueryData<{ data: ApiKey[] }>(keys.apiKeys);
      if (previous) qc.setQueryData(keys.apiKeys, { data: previous.data.filter((k) => k.id !== id) });
      return { previous };
    },
    onError: (_e, _id, ctx) => {
      if (ctx?.previous) qc.setQueryData(keys.apiKeys, ctx.previous);
    },
    onSettled: () => qc.invalidateQueries({ queryKey: keys.apiKeys }),
  });
}

/** Read a website's brand. Returns suggestions only; the caller saves what the user keeps. */
export function useImportBrand() {
  return useMutation({ mutationFn: (url: string) => accountApi.importBrand(url).then((r) => r.brand), meta: { silentError: true } });
}
