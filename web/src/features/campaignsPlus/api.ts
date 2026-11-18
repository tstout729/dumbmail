import { keepPreviousData, useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import type { Campaign, ListResponse } from "../../api";
import { api, withQuery } from "../../api/client";
import { keys as coreKeys } from "../../api/keys";
import { toast } from "../../lib/toast";
import type {
  ArchiveResult,
  CalendarData,
  CampaignCounts,
  CampaignListItem,
  CampaignSearch,
  LinkDefaults,
  LinkOverride,
  LinkParams,
  LinkTags,
  ResendInfo,
  ResendTarget,
  TagList,
  TagRef,
} from "./types";

// Under the core "campaigns" key, so anything that refreshes campaigns refreshes these too.
const keys = {
  search: (q: CampaignSearch) => [...coreKeys.campaigns, "search", q] as const,
  counts: [...coreKeys.campaigns, "counts"] as const,
  calendar: (from: string, to: string) => [...coreKeys.campaigns, "calendar", from, to] as const,
  resends: (id: string) => [...coreKeys.campaigns, "resends", id] as const,
  tags: ["tags"] as const,
  linkDefaults: ["link-tags"] as const,
  campaignLinks: (id: string) => ["link-tags", "campaign", id] as const,
  flowLinks: (id: string) => ["link-tags", "flow", id] as const,
};

export type ItemKind = "campaigns" | "flows" | "templates";

export function useCampaignSearch(q: CampaignSearch) {
  return useQuery({
    queryKey: keys.search(q),
    queryFn: () => api<ListResponse<CampaignListItem>>("GET", withQuery("/api/campaigns", q)),
    placeholderData: keepPreviousData,
  });
}

export function useCampaignCounts() {
  return useQuery({ queryKey: keys.counts, queryFn: () => api<CampaignCounts>("GET", "/api/campaigns/counts") });
}

export function useTags() {
  return useQuery({ queryKey: keys.tags, queryFn: () => api<TagList>("GET", "/api/tags") });
}

/** Add or remove tags on campaigns, flows or templates. */
export function useApplyTags(kind: ItemKind) {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (b: { ids: string[]; add?: string[]; remove?: string[] }) => api<{ data: { id: string; tags: TagRef[] }[] }>("POST", `/api/${kind}/tags`, b),
    onSettled: () => {
      void qc.invalidateQueries({ queryKey: keys.tags });
      void qc.invalidateQueries({ queryKey: [kind] });
    },
  });
}

/** Archive (or unarchive) campaigns, flows or templates; says which were skipped and why. */
export function useArchive(kind: ItemKind) {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (b: { ids: string[]; archived: boolean }) => api<ArchiveResult>("POST", `/api/${kind}/archive`, b),
    onSuccess: (r, b) => {
      const n = r.changed.length;
      if (n > 0) toast.success(`${b.archived ? "Archived" : "Unarchived"} ${n}`);
      for (const s of r.skipped.slice(0, 3)) toast.error(`Not ${b.archived ? "archived" : "unarchived"}: ${s.reason}`);
    },
    onSettled: () => void qc.invalidateQueries({ queryKey: [kind] }),
  });
}

export function useCalendar(from: string, to: string) {
  return useQuery({ queryKey: keys.calendar(from, to), queryFn: () => api<CalendarData>("GET", withQuery("/api/campaigns/calendar", { from, to })), placeholderData: keepPreviousData });
}

export function useReschedule() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: ({ id, date }: { id: string; date: string }) => api<Campaign>("POST", `/api/campaigns/${id}/reschedule`, { date }),
    onSuccess: (c) => qc.setQueryData(coreKeys.campaign(c.id), c),
    onSettled: () => void qc.invalidateQueries({ queryKey: coreKeys.campaigns }),
  });
}

export function useResendInfo(id: string, enabled = true) {
  return useQuery({ queryKey: keys.resends(id), queryFn: () => api<ResendInfo>("GET", `/api/campaigns/${id}/resends`), enabled });
}

export function useCreateResend(id: string) {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (b: { target: ResendTarget; subject?: string; preheader?: string }) => api<Campaign>("POST", `/api/campaigns/${id}/resend`, b),
    onSettled: () => void qc.invalidateQueries({ queryKey: coreKeys.campaigns }),
  });
}

export function useLinkDefaults() {
  return useQuery({ queryKey: keys.linkDefaults, queryFn: () => api<LinkDefaults>("GET", "/api/link-tags") });
}

export function useUpdateLinkDefaults() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (b: { enabled?: boolean; params?: LinkParams; skip_domains?: string[] }) => api<LinkDefaults>("PUT", "/api/link-tags", b),
    onSuccess: (d) => {
      qc.setQueryData(keys.linkDefaults, d);
      void qc.invalidateQueries({ queryKey: ["link-tags"] });
      toast.success("Link tracking saved");
    },
  });
}

export function useLinkTags(kind: "campaign" | "flow", id: string) {
  return useQuery({
    queryKey: kind === "campaign" ? keys.campaignLinks(id) : keys.flowLinks(id),
    queryFn: () => api<LinkTags>("GET", `/api/${kind}s/${id}/link-tags`),
  });
}

export function useSetLinkTags(kind: "campaign" | "flow", id: string) {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (o: LinkOverride) => api<LinkTags>("PUT", `/api/${kind}s/${id}/link-tags`, o),
    onSuccess: (t) => qc.setQueryData(kind === "campaign" ? keys.campaignLinks(id) : keys.flowLinks(id), t),
  });
}

/** Rows per page in the flow and template lists. */
export const ITEMS_PAGE = 100;

/** A page of flows or templates with this feature's fields: archived ones or the rest, searched by name. */
export function useItems<T>(kind: "flows" | "templates", q: { archived: boolean; q?: string; channel?: string; offset: number }) {
  return useQuery({
    queryKey: [kind, "with-tags", q] as const,
    queryFn: () =>
      api<ListResponse<T & { tags: TagRef[]; archived_at: string | null }>>(
        "GET",
        withQuery(`/api/${kind}`, { archived: q.archived ? "true" : undefined, q: q.q || undefined, channel: q.channel, limit: ITEMS_PAGE, offset: q.offset || undefined }),
      ),
    placeholderData: keepPreviousData,
  });
}
