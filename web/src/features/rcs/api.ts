import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { api, withQuery } from "../../api/client";
import type { RcsPreview, RcsReport, RcsSettings, RcsSettingsPatch } from "./types";

export const rcsKeys = {
  settings: ["rcs", "settings"] as const,
  report: (q: { campaign_id?: string; flow_id?: string }) => ["rcs", "report", q] as const,
};

export function useRcsSettings() {
  return useQuery({ queryKey: rcsKeys.settings, queryFn: () => api<RcsSettings>("GET", "/api/rcs/settings"), staleTime: 60_000 });
}

export function useUpdateRcsSettings() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (patch: RcsSettingsPatch) => api<RcsSettings>("PATCH", "/api/rcs/settings", patch),
    onSuccess: (s) => qc.setQueryData(rcsKeys.settings, s),
  });
}

export function useCheckRcsAgent() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: () => api<RcsSettings>("POST", "/api/rcs/settings/check"),
    onSuccess: (s) => qc.setQueryData(rcsKeys.settings, s),
  });
}

export function useRcsReport(q: { campaign_id?: string; flow_id?: string }) {
  return useQuery({ queryKey: rcsKeys.report(q), queryFn: () => api<RcsReport>("GET", withQuery("/api/rcs/report", q)) });
}

/** The rich version in a server render (`/api/render`), when there is one. */
export function rcsPreviewOf(render: object | undefined): RcsPreview | null {
  if (!render || !("rcs" in render)) return null;
  const v = (render as { rcs: unknown }).rcs;
  return v && typeof v === "object" && "suggestions" in v ? (v as RcsPreview) : null;
}
