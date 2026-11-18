import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { api } from "../../api/client";
import { toast } from "../../lib/toast";
import type { ShortLinkSettings } from "./types";

const keys = { settings: ["short-links", "settings"] as const };

export function useShortLinkSettings() {
  return useQuery({ queryKey: keys.settings, queryFn: () => api<ShortLinkSettings>("GET", "/api/short-links/settings"), staleTime: 60_000 });
}

export function useUpdateShortLinkSettings() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (patch: Pick<ShortLinkSettings, "enabled">) => api<ShortLinkSettings>("PATCH", "/api/short-links/settings", patch),
    onSuccess: (s) => {
      qc.setQueryData(keys.settings, s);
      toast.success(s.enabled ? "Links in texts will be shortened" : "Links in texts won't be shortened");
    },
  });
}
