import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { api } from "./client";
import { keys } from "./keys";
import type { Account, AccountPatch, ApiKey, Billing, CreatedApiKey, SendingStatus, Session } from "./types";

export const accountApi = {
  update: (patch: AccountPatch) => api<Account>("PATCH", "/api/account", patch),
  sending: () => api<SendingStatus>("GET", "/api/account/sending"),
  setDomain: (domain: string) => api<SendingStatus>("POST", "/api/account/domain", { domain }),
  verifyDomain: () => api<SendingStatus>("POST", "/api/account/domain/verify"),
  apiKeys: () => api<{ data: ApiKey[] }>("GET", "/api/api-keys"),
  createApiKey: (name: string) => api<CreatedApiKey>("POST", "/api/api-keys", { name }),
  revokeApiKey: (id: string) => api<{ ok: true }>("DELETE", `/api/api-keys/${id}`),
  billing: () => api<Billing>("GET", "/api/billing"),
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

export function useSetDomain() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: accountApi.setDomain,
    onSuccess: (data) => qc.setQueryData(keys.sending, data),
  });
}

export function useVerifyDomain() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: accountApi.verifyDomain,
    onSuccess: (data) => qc.setQueryData(keys.sending, data),
  });
}

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

export function useBilling() {
  return useQuery({ queryKey: keys.billing, queryFn: accountApi.billing });
}
