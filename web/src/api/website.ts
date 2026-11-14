import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { api } from "./client";
import { keys } from "./keys";
import type { WebsiteCheck, WebsiteStatus } from "./types";

export const websiteApi = {
  get: () => api<WebsiteStatus>("GET", "/api/website"),
  check: (url: string) => api<WebsiteCheck>("POST", "/api/website/check", { url }),
};

export function useWebsite() {
  return useQuery({ queryKey: keys.website, queryFn: websiteApi.get });
}

export function useCheckWebsite() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: websiteApi.check,
    // A successful check may also have recorded the site as installed.
    onSuccess: () => qc.invalidateQueries({ queryKey: keys.website }),
  });
}
