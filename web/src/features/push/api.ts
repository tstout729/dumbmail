import { keepPreviousData, useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { api, withQuery } from "../../api/client";
import type { PushContent, PushRender, PushReport, PushSettings, PushSettingsPatch, PushTestResult } from "./types";

const keys = {
  settings: ["push", "settings"] as const,
  render: (content: PushContent | null) => ["push", "render", content] as const,
  report: (q: { campaign_id?: string; flow_id?: string }) => ["push", "report", q] as const,
};

export function usePushSettings() {
  return useQuery({ queryKey: keys.settings, queryFn: () => api<PushSettings>("GET", "/api/push/settings") });
}

export function useUpdatePushSettings() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (patch: PushSettingsPatch) => api<PushSettings>("PATCH", "/api/push/settings", patch),
    onSuccess: (s) => qc.setQueryData(keys.settings, s),
  });
}

export function useRotateIdentitySecret() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: () => api<{ identity_secret: string; note: string }>("POST", "/api/push/identity-secret"),
    onSuccess: () => void qc.invalidateQueries({ queryKey: keys.settings }),
  });
}

/** Add this browser's push subscription as the signed-in member's own test device. */
export function useAddTestDevice() {
  return useMutation({ mutationFn: (subscription: PushSubscriptionJSON) => api<{ device_id: string }>("POST", "/api/push/test-devices", { subscription }) });
}

export type PushTestInput = { content?: PushContent; to?: string; profile_id?: string; device_id?: string };

export function useSendTestPush() {
  return useMutation({ mutationFn: (input: PushTestInput) => api<PushTestResult>("POST", "/api/push/test", input) });
}

/** The notification as a real recipient (or the sample person) would see it. */
export function usePushRender(content: PushContent | null) {
  return useQuery({
    queryKey: keys.render(content),
    queryFn: () => api<PushRender>("POST", "/api/render", { channel: "push", content }),
    enabled: content !== null && (content.title.trim() !== "" || content.body.trim() !== ""),
    placeholderData: keepPreviousData,
    retry: false,
  });
}

export function usePushReport(q: { campaign_id?: string; flow_id?: string }) {
  return useQuery({ queryKey: keys.report(q), queryFn: () => api<PushReport>("GET", withQuery("/api/push/report", q)) });
}
