import { keepPreviousData, useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { api, withQuery } from "../../api/client";
import type {
  AttributionPatch,
  AttributionPreview,
  AttributionSettings,
  Benchmarks,
  Catalog,
  Cohorts,
  Deliverability,
  ExportFile,
  ExportRequest,
  Funnel,
  FunnelQuery,
  Member,
  ReportInput,
  ReportList,
  ReportResult,
  ReportSpec,
  SavedReport,
} from "./types";

export const reportKeys = {
  all: ["reports"] as const,
  catalog: ["reports", "catalog"] as const,
  list: ["reports", "list"] as const,
  report: (id: string) => ["reports", "report", id] as const,
  run: (spec: ReportSpec) => ["reports", "run", spec] as const,
  members: ["reports", "members"] as const,
  attribution: ["reports", "attribution"] as const,
  preview: (patch: AttributionPatch, days: number) => ["reports", "attribution-preview", patch, days] as const,
  cohorts: (by: string, months: number) => ["reports", "cohorts", by, months] as const,
  funnel: (q: FunnelQuery) => ["reports", "funnel", q] as const,
  deliverability: (days: number) => ["reports", "deliverability", days] as const,
  benchmarks: (industry: string) => ["reports", "benchmarks", industry] as const,
  exports: ["reports", "exports"] as const,
};

export const reportsApi = {
  catalog: () => api<Catalog>("GET", "/api/reports/catalog"),
  list: () => api<ReportList>("GET", "/api/reports"),
  get: (id: string) => api<SavedReport>("GET", `/api/reports/${encodeURIComponent(id)}`),
  run: (spec: ReportSpec) => api<ReportResult>("POST", "/api/reports/run", spec),
  create: (input: ReportInput) => api<SavedReport>("POST", "/api/reports", input),
  update: (id: string, input: ReportInput) => api<SavedReport>("PATCH", `/api/reports/${encodeURIComponent(id)}`, input),
  duplicate: (id: string, name?: string) => api<SavedReport>("POST", `/api/reports/${encodeURIComponent(id)}/duplicate`, name ? { name } : {}),
  remove: (id: string) => api<null>("DELETE", `/api/reports/${encodeURIComponent(id)}`),
  members: () => api<{ data: Member[] }>("GET", "/api/reports/members"),
  attribution: () => api<AttributionSettings>("GET", "/api/reports/attribution"),
  saveAttribution: (patch: AttributionPatch) => api<AttributionSettings>("PATCH", "/api/reports/attribution", patch),
  previewAttribution: (patch: AttributionPatch, days: number) => api<AttributionPreview>("POST", "/api/reports/attribution/preview", { settings: patch, days }),
  cohorts: (by: string, months: number) => api<Cohorts>("GET", withQuery("/api/reports/cohorts", { by, months })),
  funnel: (q: FunnelQuery) => api<Funnel>("POST", "/api/reports/funnel", q),
  deliverability: (days: number) => api<Deliverability>("GET", withQuery("/api/reports/deliverability", { days })),
  benchmarks: (industry: string) => api<Benchmarks>("GET", withQuery("/api/reports/benchmarks", { industry })),
  exports: () => api<{ data: ExportFile[] }>("GET", "/api/reports/exports"),
  startExport: (r: ExportRequest) => api<ExportFile>("POST", "/api/reports/exports", r),
};

/** Fetch a CSV from the API and save it, keeping the server's filename. */
export async function downloadCsv(path: string, body?: unknown, fallbackName = "report.csv") {
  const res = await fetch(path, {
    method: body === undefined ? "GET" : "POST",
    credentials: "include",
    headers: body === undefined ? undefined : { "Content-Type": "application/json" },
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  if (!res.ok) {
    let message = `Download failed (${res.status})`;
    try {
      const j = (await res.json()) as { error?: { message?: string } };
      message = j.error?.message ?? message;
    } catch {
      // Not JSON; keep the status message.
    }
    throw new Error(message);
  }
  const name = /filename="([^"]+)"/.exec(res.headers.get("content-disposition") ?? "")?.[1] ?? fallbackName;
  const url = URL.createObjectURL(await res.blob());
  const a = document.createElement("a");
  a.href = url;
  a.download = name;
  a.click();
  URL.revokeObjectURL(url);
}

export function useCatalog() {
  return useQuery({ queryKey: reportKeys.catalog, queryFn: reportsApi.catalog, staleTime: Infinity });
}

export function useReportList() {
  return useQuery({ queryKey: reportKeys.list, queryFn: reportsApi.list });
}

export function useSavedReport(id: string | undefined) {
  return useQuery({ queryKey: reportKeys.report(id ?? ""), queryFn: () => reportsApi.get(id as string), enabled: Boolean(id) });
}

export function useRunReport(spec: ReportSpec | null) {
  return useQuery({
    queryKey: reportKeys.run(spec ?? { source: "" }),
    queryFn: () => reportsApi.run(spec as ReportSpec),
    enabled: spec !== null,
    placeholderData: keepPreviousData,
  });
}

export function useMembers() {
  return useQuery({ queryKey: reportKeys.members, queryFn: reportsApi.members, select: (r) => r.data });
}

export function useCreateReport() {
  const qc = useQueryClient();
  return useMutation({ mutationFn: reportsApi.create, onSuccess: () => qc.invalidateQueries({ queryKey: reportKeys.list }) });
}

export function useUpdateReport(id: string) {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (input: ReportInput) => reportsApi.update(id, input),
    onSuccess: (r) => {
      qc.setQueryData(reportKeys.report(id), r);
      qc.invalidateQueries({ queryKey: reportKeys.list });
    },
  });
}

export function useDuplicateReport() {
  const qc = useQueryClient();
  return useMutation({ mutationFn: ({ id, name }: { id: string; name?: string }) => reportsApi.duplicate(id, name), onSuccess: () => qc.invalidateQueries({ queryKey: reportKeys.list }) });
}

export function useDeleteReport() {
  const qc = useQueryClient();
  return useMutation({ mutationFn: reportsApi.remove, onSuccess: () => qc.invalidateQueries({ queryKey: reportKeys.list }) });
}

export function useAttribution() {
  return useQuery({ queryKey: reportKeys.attribution, queryFn: reportsApi.attribution });
}

export function useSaveAttribution() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: reportsApi.saveAttribution,
    onSuccess: (s) => {
      qc.setQueryData(reportKeys.attribution, s);
      // Every revenue number in the dashboard changes with the settings.
      qc.invalidateQueries();
    },
  });
}

export function useAttributionPreview(patch: AttributionPatch | null, days: number) {
  return useQuery({
    queryKey: reportKeys.preview(patch ?? {}, days),
    queryFn: () => reportsApi.previewAttribution(patch as AttributionPatch, days),
    enabled: patch !== null,
    placeholderData: keepPreviousData,
  });
}

export function useCohorts(by: string, months: number) {
  return useQuery({ queryKey: reportKeys.cohorts(by, months), queryFn: () => reportsApi.cohorts(by, months), placeholderData: keepPreviousData });
}

export function useFunnel(q: FunnelQuery | null) {
  return useQuery({ queryKey: reportKeys.funnel(q ?? { steps: [] }), queryFn: () => reportsApi.funnel(q as FunnelQuery), enabled: q !== null, placeholderData: keepPreviousData });
}

export function useDeliverability(days: number) {
  return useQuery({ queryKey: reportKeys.deliverability(days), queryFn: () => reportsApi.deliverability(days), placeholderData: keepPreviousData });
}

export function useBenchmarks(industry: string) {
  return useQuery({ queryKey: reportKeys.benchmarks(industry), queryFn: () => reportsApi.benchmarks(industry), placeholderData: keepPreviousData });
}

export function useExports() {
  return useQuery({
    queryKey: reportKeys.exports,
    queryFn: reportsApi.exports,
    select: (r) => r.data,
    // Poll while a file is being written.
    refetchInterval: (q) => (q.state.data?.data.some((e) => e.status === "queued" || e.status === "running") ? 2000 : false),
  });
}

export function useStartExport() {
  const qc = useQueryClient();
  return useMutation({ mutationFn: reportsApi.startExport, onSuccess: () => qc.invalidateQueries({ queryKey: reportKeys.exports }) });
}
