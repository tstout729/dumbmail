import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { api, withQuery } from "../../api/client";
import type { DatasetInfo, Destination, DestinationInput, Run } from "./types";

const keys = {
  all: ["warehouse"] as const,
  datasets: ["warehouse", "datasets"] as const,
  runs: (id: string) => ["warehouse", id, "runs"] as const,
};

export const warehouseApi = {
  datasets: () => api<{ data: DatasetInfo[] }>("GET", "/api/warehouse/datasets"),
  list: () => api<{ data: Destination[] }>("GET", "/api/warehouse/destinations"),
  create: (input: DestinationInput) => api<Destination>("POST", "/api/warehouse/destinations", input),
  update: (id: string, input: DestinationInput) => api<Destination>("PATCH", `/api/warehouse/destinations/${id}`, input),
  remove: (id: string) => api<{ ok: boolean }>("DELETE", `/api/warehouse/destinations/${id}`),
  test: (id: string) => api<{ ok: boolean; error?: string }>("POST", `/api/warehouse/destinations/${id}/test`),
  start: (id: string, kind: "incremental" | "full") => api<Run>("POST", `/api/warehouse/destinations/${id}/runs`, { kind }),
  runs: (id: string) => api<{ data: Run[] }>("GET", withQuery(`/api/warehouse/destinations/${id}/runs`, { limit: 20 })),
};

const active = (r: Run | null | undefined) => r?.status === "queued" || r?.status === "running";

export function useDatasets() {
  return useQuery({ queryKey: keys.datasets, queryFn: warehouseApi.datasets, select: (r) => r.data, staleTime: 60 * 60_000 });
}

export function useDestinations() {
  // A run in progress reports rows as it goes; refresh while one runs.
  return useQuery({ queryKey: keys.all, queryFn: warehouseApi.list, select: (r) => r.data, refetchInterval: (q) => (q.state.data?.data.some((d) => active(d.last_run)) ? 4000 : 60_000) });
}

export function useRuns(id: string) {
  return useQuery({ queryKey: keys.runs(id), queryFn: () => warehouseApi.runs(id), select: (r) => r.data, refetchInterval: (q) => (q.state.data?.data.some(active) ? 4000 : false) });
}

function useInvalidating<V, R>(fn: (v: V) => Promise<R>, silent = false) {
  const qc = useQueryClient();
  return useMutation({ mutationFn: fn, meta: { silentError: silent }, onSuccess: () => qc.invalidateQueries({ queryKey: keys.all }) });
}

export function useCreateDestination() {
  return useInvalidating(warehouseApi.create, true);
}

export function useUpdateDestination() {
  return useInvalidating(({ id, input }: { id: string; input: DestinationInput }) => warehouseApi.update(id, input), true);
}

export function useDeleteDestination() {
  return useInvalidating(warehouseApi.remove);
}

export function useTestDestination() {
  return useInvalidating(warehouseApi.test);
}

export function useStartRun() {
  return useInvalidating(({ id, kind }: { id: string; kind: "incremental" | "full" }) => warehouseApi.start(id, kind));
}
