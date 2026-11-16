import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { api } from "./client";
import { keys } from "./keys";
import type { SiteTest, WebsiteCheck, WebsiteStatus } from "./types";

export const websiteApi = {
  get: () => api<WebsiteStatus>("GET", "/api/website"),
  check: (url: string) => api<WebsiteCheck>("POST", "/api/website/check", { url }),
  startTest: (url: string) => api<SiteTest>("POST", "/api/website/tests", { url }),
  test: (code: string) => api<SiteTest>("GET", `/api/website/tests/${encodeURIComponent(code)}`),
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

export function useStartSiteTest() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: websiteApi.startTest,
    onSuccess: (t) => qc.setQueryData(keys.siteTest(t.code), t),
  });
}

/** Results of a running setup test, refreshed every 2 seconds until it expires. */
export function useSiteTest(code: string | null) {
  const qc = useQueryClient();
  return useQuery({
    queryKey: keys.siteTest(code ?? ""),
    queryFn: async () => {
      const before = qc.getQueryData<SiteTest>(keys.siteTest(code ?? ""));
      const t = await websiteApi.test(code ?? "");
      // The first report from the site also marks the snippet as installed.
      if (t.checks.loaded && !before?.checks.loaded) void qc.invalidateQueries({ queryKey: keys.website });
      return t;
    },
    enabled: code !== null,
    refetchInterval: (q) => (q.state.data && !q.state.data.active ? false : 2000),
  });
}
