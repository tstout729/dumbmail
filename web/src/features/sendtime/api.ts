import { keepPreviousData, useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { api } from "../../api/client";
import { toast } from "../../lib/toast";
import type { SendPlanInput, SendPlanPreview, SendProgress, SendSettings } from "./types";

const keys = {
  preview: (input: SendPlanInput | null) => ["sendtime", "preview", input] as const,
  progress: (id: string) => ["sendtime", "progress", id] as const,
  settings: ["sendtime", "settings"] as const,
};

/** When a send would go out, estimated from a sample of the audience. */
export function useSendPreview(input: SendPlanInput | null) {
  return useQuery({
    queryKey: keys.preview(input),
    queryFn: () => api<SendPlanPreview>("POST", "/api/sendtime/preview", input),
    enabled: input !== null && input.audience.include.length > 0,
    placeholderData: keepPreviousData,
    staleTime: 30_000,
  });
}

/** A campaign's send by release time. Polls while it's going out. */
export function useSendProgress(id: string, live: boolean) {
  return useQuery({
    queryKey: keys.progress(id),
    queryFn: () => api<SendProgress>("GET", `/api/campaigns/${id}/send-progress`),
    refetchInterval: live ? 15_000 : false,
  });
}

export function useSendSettings() {
  return useQuery({ queryKey: keys.settings, queryFn: () => api<SendSettings>("GET", "/api/sendtime/settings") });
}

export function useUpdateSendSettings() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (patch: Partial<Pick<SendSettings, "sms_window_start" | "sms_window_end" | "sms_zone_conflict">>) => api<SendSettings>("PATCH", "/api/sendtime/settings", patch),
    onSuccess: (s) => {
      qc.setQueryData(keys.settings, s);
      toast.success("Text message settings saved");
    },
  });
}
