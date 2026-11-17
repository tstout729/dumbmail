import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { api, withQuery } from "../../api/client";
import type { KlaviyoCampaign, KlaviyoIssue, KlaviyoReport, KlaviyoState, StartOptions } from "./types";

const key = ["klaviyo-import"] as const;
const busy = new Set(["surveying", "importing", "syncing"]);

/** The import's state, refreshed every 3 seconds while something runs in the background. */
export function useKlaviyo() {
  return useQuery({
    queryKey: key,
    queryFn: () => api<KlaviyoState>("GET", "/api/klaviyo"),
    refetchInterval: (q) => (q.state.data?.import && busy.has(q.state.data.import.status) ? 3000 : false),
  });
}

export function useKlaviyoReport(enabled: boolean, version: string | null) {
  return useQuery({ queryKey: [...key, "report", version], queryFn: () => api<KlaviyoReport>("GET", "/api/klaviyo/report"), enabled });
}

export function useKlaviyoIssues(enabled: boolean, version: string | null) {
  return useQuery({
    queryKey: [...key, "issues", version],
    queryFn: () => api<{ data: KlaviyoIssue[]; total: number }>("GET", withQuery("/api/klaviyo/issues", { limit: 500 })),
    enabled,
  });
}

export function useKlaviyoCampaigns(enabled: boolean, version: string | null) {
  return useQuery({
    queryKey: [...key, "campaigns", version],
    queryFn: () => api<{ data: KlaviyoCampaign[]; total: number }>("GET", withQuery("/api/klaviyo/campaigns", { limit: 200 })),
    enabled,
  });
}

function useAction<T>(fn: (v: T) => Promise<KlaviyoState>) {
  const qc = useQueryClient();
  return useMutation({ mutationFn: fn, onSuccess: (s) => qc.setQueryData(key, s) });
}

export const useConnectKlaviyo = () => useAction((api_key: string) => api<KlaviyoState>("POST", "/api/klaviyo/connect", { api_key }));
export const useStartKlaviyo = () => useAction((o: StartOptions) => api<KlaviyoState>("POST", "/api/klaviyo/start", o));
export const usePauseKlaviyo = () => useAction((resume: boolean) => api<KlaviyoState>("POST", resume ? "/api/klaviyo/resume" : "/api/klaviyo/pause"));
export const useCancelKlaviyo = () => useAction(() => api<KlaviyoState>("POST", "/api/klaviyo/cancel"));
export const useKlaviyoSync = () => useAction((enabled: boolean) => api<KlaviyoState>("POST", "/api/klaviyo/sync", { enabled }));
export const useKlaviyoSyncNow = () => useAction(() => api<KlaviyoState>("POST", "/api/klaviyo/sync-now"));
export const useMarkKlaviyo = () => useAction((v: { item: string; done: boolean }) => api<KlaviyoState>("POST", "/api/klaviyo/checklist", v));
export const useCheckSiteKlaviyo = () => useAction(() => api<KlaviyoState>("POST", "/api/klaviyo/checklist/check-site"));
