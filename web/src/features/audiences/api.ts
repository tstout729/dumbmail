import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { api } from "../../api/client";
import type { AdAccount, AdConnection, AdPlatform, AdSettings, AudienceSync, AudienceSyncDetail, CreateSyncInput, UpdateSyncInput } from "./types";

const base = "/api/ad-audiences";

export const adKeys = {
  connections: ["ad-audiences", "connections"] as const,
  accounts: (platform: AdPlatform) => ["ad-audiences", "accounts", platform] as const,
  syncs: ["ad-audiences", "syncs"] as const,
  sync: (id: string) => ["ad-audiences", "sync", id] as const,
};

const adApi = {
  connections: () => api<{ data: AdConnection[]; settings: AdSettings; connect_url: string }>("GET", `${base}/connections`),
  saveSettings: (input: { google_consent: boolean }) => api<AdSettings>("PATCH", `${base}/settings`, input),
  start: (platform: AdPlatform) => api<{ url: string }>("POST", `${base}/connections/${platform}/start`),
  disconnect: (platform: AdPlatform) => api<{ disconnected: boolean; revoked: boolean; note: string }>("DELETE", `${base}/connections/${platform}`),
  accounts: (platform: AdPlatform) => api<{ data: AdAccount[]; manual_entry: boolean }>("GET", `${base}/connections/${platform}/accounts`),
  syncs: () => api<{ data: AudienceSync[] }>("GET", `${base}/syncs`),
  sync: (id: string) => api<AudienceSyncDetail>("GET", `${base}/syncs/${id}`),
  create: (input: CreateSyncInput) => api<AudienceSyncDetail>("POST", `${base}/syncs`, input),
  update: (id: string, input: UpdateSyncInput) => api<AudienceSyncDetail>("PATCH", `${base}/syncs/${id}`, input),
  run: (id: string) => api<AudienceSyncDetail & { started: boolean }>("POST", `${base}/syncs/${id}/run`),
  remove: (id: string, deleteOnPlatform: boolean) =>
    api<{ deleted: boolean; note?: string }>("DELETE", `${base}/syncs/${id}?delete_on_platform=${deleteOnPlatform}&confirm=true`),
};

/** Poll every few seconds while a sync runs, so its numbers fill in; less often while one is being removed. */
const whileRunning = (running: boolean, removing = false) => (running ? 3000 : removing ? 15000 : false);

export function useAdConnections() {
  return useQuery({ queryKey: adKeys.connections, queryFn: adApi.connections });
}

export function useAdAccounts(platform: AdPlatform | null) {
  return useQuery({
    queryKey: adKeys.accounts(platform ?? "meta"),
    queryFn: () => adApi.accounts(platform ?? "meta"),
    enabled: platform !== null,
  });
}

export function useAudienceSyncs() {
  return useQuery({
    queryKey: adKeys.syncs,
    queryFn: adApi.syncs,
    select: (r) => r.data,
    refetchInterval: (q) => {
      const all = q.state.data?.data ?? [];
      return whileRunning(all.some((s) => s.running), all.some((s) => s.removing));
    },
  });
}

export function useAudienceSync(id: string) {
  return useQuery({ queryKey: adKeys.sync(id), queryFn: () => adApi.sync(id), refetchInterval: (q) => (q.state.error ? false : whileRunning(q.state.data?.running ?? false, q.state.data?.removing ?? false)) });
}

function useInvalidate() {
  const qc = useQueryClient();
  return () => qc.invalidateQueries({ queryKey: ["ad-audiences"] });
}

export function useStartConnect() {
  return useMutation({ mutationFn: adApi.start });
}

export function useSaveAdSettings() {
  const invalidate = useInvalidate();
  return useMutation({ mutationFn: adApi.saveSettings, onSuccess: invalidate });
}

export function useDisconnect() {
  const invalidate = useInvalidate();
  return useMutation({ mutationFn: adApi.disconnect, onSuccess: invalidate });
}

export function useCreateSync() {
  const invalidate = useInvalidate();
  return useMutation({ mutationFn: adApi.create, onSuccess: invalidate, meta: { silentError: true } });
}

export function useUpdateSync(id: string) {
  const invalidate = useInvalidate();
  return useMutation({ mutationFn: (input: UpdateSyncInput) => adApi.update(id, input), onSuccess: invalidate });
}

export function useRunSync() {
  const invalidate = useInvalidate();
  return useMutation({ mutationFn: adApi.run, onSuccess: invalidate });
}

export function useDeleteSync() {
  const invalidate = useInvalidate();
  return useMutation({ mutationFn: (v: { id: string; deleteOnPlatform: boolean }) => adApi.remove(v.id, v.deleteOnPlatform), onSuccess: invalidate });
}
