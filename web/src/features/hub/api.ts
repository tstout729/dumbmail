import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { api } from "../../api/client";
import type { HubSettings, HubSettingsPatch } from "./types";

const keys = {
  settings: ["hub", "settings"] as const,
  preview: ["hub", "preview"] as const,
};

export function useHubSettings() {
  return useQuery({ queryKey: keys.settings, queryFn: () => api<HubSettings>("GET", "/api/hub/settings") });
}

export function useUpdateHubSettings() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (p: HubSettingsPatch) => api<HubSettings>("PATCH", "/api/hub/settings", p),
    onSuccess: (s) => {
      qc.setQueryData(keys.settings, s);
      void qc.invalidateQueries({ queryKey: keys.preview });
    },
  });
}

/** The hub as a made-up shopper would see it with the saved settings. */
export function useHubPreview() {
  return useQuery({ queryKey: keys.preview, queryFn: () => api<{ html: string }>("GET", "/api/hub/preview") });
}
