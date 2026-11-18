import { keepPreviousData, useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { api, withQuery } from "../../api/client";
import type { Backfill, BackfillDryRun, FlowResults, NotifyAddressStatus, WaitingPage } from "./types";

const keys = {
  results: (id: string, days: number) => ["flowsPlus", "results", id, days] as const,
  waiting: (id: string, node: string, after: string | null) => ["flowsPlus", "waiting", id, node, after] as const,
  backfills: (id: string) => ["flowsPlus", "backfills", id] as const,
  dryRun: (id: string) => ["flowsPlus", "dryRun", id] as const,
  addresses: (emails: string) => ["flowsPlus", "addresses", emails] as const,
  trust: (metric: string) => ["flowsPlus", "trust", metric] as const,
};

/** Per-step results over the last `days` days. */
export function useFlowResults(id: string, days: number, enabled: boolean) {
  return useQuery({
    queryKey: keys.results(id, days),
    queryFn: () => api<FlowResults>("GET", withQuery(`/api/flows/${id}/results`, { days })),
    enabled,
    placeholderData: keepPreviousData,
    staleTime: 30_000,
  });
}

export function useWaiting(id: string, node: string, after: string | null) {
  return useQuery({
    queryKey: keys.waiting(id, node, after),
    queryFn: () => api<WaitingPage>("GET", withQuery(`/api/flows/${id}/steps/${encodeURIComponent(node)}/waiting`, { limit: 50, after })),
    placeholderData: keepPreviousData,
  });
}

export function useRemoveFromFlow(id: string) {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (runIds: string[]) => api<{ removed: number }>("POST", `/api/flows/${id}/runs/remove`, { run_ids: runIds, confirm: true }),
    onSuccess: () => qc.invalidateQueries({ queryKey: ["flowsPlus"] }),
  });
}

/** How many people a back-populate would add (nothing is changed). */
export function useBackfillDryRun(id: string, enabled: boolean) {
  return useQuery({
    queryKey: keys.dryRun(id),
    queryFn: () => api<BackfillDryRun>("POST", `/api/flows/${id}/backfill`, { confirm: false }),
    enabled,
    retry: false,
  });
}

export function useBackfills(id: string, poll: boolean) {
  return useQuery({
    queryKey: keys.backfills(id),
    queryFn: () => api<{ data: Backfill[] }>("GET", `/api/flows/${id}/backfills`),
    select: (r) => r.data,
    refetchInterval: poll ? 3_000 : false,
  });
}

export function useStartBackfill(id: string) {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: () => api<Backfill>("POST", `/api/flows/${id}/backfill`, { confirm: true }),
    onSuccess: () => qc.invalidateQueries({ queryKey: ["flowsPlus"] }),
  });
}

export function useCancelBackfill(id: string) {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (backfillId: string) => api<Backfill>("POST", `/api/flows/${id}/backfills/${backfillId}/cancel`),
    onSuccess: () => qc.invalidateQueries({ queryKey: ["flowsPlus"] }),
  });
}

/** Whether each address may get notifications (team, confirmed) or still has to confirm. */
export function useNotifyAddresses(emails: string[]) {
  const joined = [...emails].sort().join(",");
  return useQuery({
    queryKey: keys.addresses(joined),
    queryFn: () => api<{ statuses: { email: string; status: NotifyAddressStatus }[] }>("GET", withQuery("/api/flows-plus/notify-addresses", { emails: joined })),
    select: (r) => r.statuses,
    enabled: joined.length > 0,
    placeholderData: keepPreviousData,
  });
}

/** Email an outside address a link to confirm (needs the permission to change account settings). */
export function useSendConfirmation() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (email: string) => api<{ email: string; sent: boolean }>("POST", "/api/flows-plus/notify-addresses/send-confirmation", { email, confirm: true }),
    onSuccess: () => qc.invalidateQueries({ queryKey: ["flowsPlus", "addresses"] }),
  });
}

/** Let a bounced address get notifications again (Admins and Owners). */
export function useClearBounce() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (email: string) => api<{ cleared: boolean }>("POST", "/api/flows-plus/notify-addresses/clear-bounce", { email }),
    onSuccess: () => qc.invalidateQueries({ queryKey: ["flowsPlus", "addresses"] }),
  });
}

/** Whether the website script can send this metric (so its values can't be trusted). */
export function useMetricTrust(metric: string | null) {
  return useQuery({
    queryKey: keys.trust(metric ?? ""),
    queryFn: () => api<{ browser_can_send: boolean }>("GET", withQuery("/api/flows-plus/metric-trust", { metric: metric ?? "" })),
    enabled: !!metric,
    staleTime: 10 * 60_000,
  });
}
