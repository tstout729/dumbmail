import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { api } from "./client";
import { keys } from "./keys";
import type { Account, AccountPatch, ApiKey, CreatedApiKey, Sending, Session } from "./types";

export const accountApi = {
  update: (patch: AccountPatch) => api<Account>("PATCH", "/api/account", patch),
  sending: () => api<Sending>("GET", "/api/account/sending"),
  setDomain: (domain: string) => api<Sending>("POST", "/api/account/domain", { domain }),
  verifyDomain: () => api<Sending>("POST", "/api/account/domain/verify"),
  removeDomain: () => api<Sending>("DELETE", "/api/account/domain"),
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

export const useSetDomain = () => useSendingMutation<string>(accountApi.setDomain);
export const useVerifyDomain = () => useSendingMutation<void>(accountApi.verifyDomain);
export const useRemoveDomain = () => useSendingMutation<void>(accountApi.removeDomain);
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
