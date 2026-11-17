import { keepPreviousData, useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { api, keys } from "../../api";
import { withQuery } from "../../api/client";
import type {
  BulkAction,
  BulkDryRun,
  BulkInput,
  Channel,
  DeleteResult,
  DuplicateGroup,
  ExportResult,
  MergeInput,
  MergePreview,
  PeopleSummary,
  PersonInput,
  PrivacyRequest,
  ProfileNote,
  SuppressionList,
  SuppressionQuery,
  SuppressResult,
} from "./types";

export const peopleKeys = {
  all: ["people"] as const,
  summary: (id: string) => ["people", "summary", id] as const,
  suppressions: (q: SuppressionQuery) => ["people", "suppressions", q] as const,
  duplicates: ["people", "duplicates"] as const,
  bulk: (id: string) => ["people", "bulk", id] as const,
  bulkRecent: ["people", "bulk"] as const,
  requests: ["people", "privacy-requests"] as const,
};

/**
 * Refresh everything that shows profiles after a change made here. Bulk-action progress isn't
 * included: it's refreshed from its own query, which mustn't cancel itself.
 */
function useRefreshAll() {
  const qc = useQueryClient();
  return () => {
    for (const queryKey of [["people", "summary"], ["people", "suppressions"], peopleKeys.duplicates, peopleKeys.requests, keys.profilesAll, ["profile"], keys.lists]) {
      void qc.invalidateQueries({ queryKey });
    }
  };
}

export function usePeopleSummary(profileId: string) {
  return useQuery({ queryKey: peopleKeys.summary(profileId), queryFn: () => api<PeopleSummary>("GET", `/api/people/profiles/${profileId}`) });
}

export function useConsentPage(profileId: string, offset: number, enabled: boolean) {
  return useQuery({
    queryKey: [...peopleKeys.summary(profileId), "history", offset],
    queryFn: () => api<{ data: PeopleSummary["consent_history"]; total: number }>("GET", withQuery(`/api/people/profiles/${profileId}/consent`, { limit: 50, offset })),
    enabled,
    placeholderData: keepPreviousData,
  });
}

export function useSetAdsOptOut(profileId: string) {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (ads_opt_out: boolean) => api<{ ads_opt_out: boolean }>("PATCH", `/api/people/profiles/${profileId}/privacy`, { ads_opt_out }),
    onSettled: () => qc.invalidateQueries({ queryKey: peopleKeys.summary(profileId) }),
  });
}

export function useAddNote(profileId: string) {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (body: string) => api<ProfileNote>("POST", `/api/people/profiles/${profileId}/notes`, { body }),
    onSuccess: () => qc.invalidateQueries({ queryKey: peopleKeys.summary(profileId) }),
  });
}

export function useDeleteNote(profileId: string) {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (noteId: string) => api<{ ok: true }>("DELETE", `/api/people/profiles/${profileId}/notes/${noteId}`),
    onSuccess: () => qc.invalidateQueries({ queryKey: peopleKeys.summary(profileId) }),
  });
}

export function useSuppressions(q: SuppressionQuery) {
  return useQuery({
    queryKey: peopleKeys.suppressions(q),
    queryFn: () => api<SuppressionList>("GET", withQuery("/api/people/suppressions", q)),
    placeholderData: keepPreviousData,
  });
}

export function suppressionsExportUrl(q: SuppressionQuery): string {
  const { limit: _limit, offset: _offset, ...filters } = q;
  return withQuery("/api/people/suppressions/export", filters);
}

export function useSuppress() {
  const refresh = useRefreshAll();
  return useMutation({
    mutationFn: (input: { addresses?: string[]; csv?: string; confirm: boolean }) => api<SuppressResult>("POST", "/api/people/suppressions", input),
    meta: { silentError: true },
    onSuccess: (r) => {
      if (!r.dry_run) refresh();
    },
  });
}

export function useUnsuppress() {
  const refresh = useRefreshAll();
  return useMutation({
    mutationFn: (input: { profile_id: string; channel: Channel }) => api<{ ok: true }>("POST", "/api/people/suppressions/unsuppress", { ...input, confirm: true }),
    onSuccess: refresh,
  });
}

export function useDuplicates() {
  return useQuery({ queryKey: peopleKeys.duplicates, queryFn: () => api<{ data: DuplicateGroup[] }>("GET", "/api/people/duplicates?limit=100") });
}

export function useMergePreview(input: Omit<MergeInput, "confirm"> | null) {
  return useQuery({
    queryKey: ["merge-preview", input],
    queryFn: () => api<MergePreview>("POST", "/api/people/merge", { ...input, confirm: false }),
    enabled: input !== null,
    placeholderData: keepPreviousData,
  });
}

export function useMerge() {
  const refresh = useRefreshAll();
  return useMutation({
    mutationFn: (input: MergeInput) => api<MergePreview>("POST", "/api/people/merge", { ...input, confirm: true }),
    onSuccess: refresh,
  });
}

export function useBulkCount(input: BulkInput | null) {
  return useQuery({
    queryKey: ["bulk-count", input],
    queryFn: () => api<BulkDryRun>("POST", "/api/people/bulk", { ...input, confirm: false }),
    enabled: input !== null,
    staleTime: 0,
  });
}

export function useStartBulk() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (input: BulkInput) => api<BulkAction>("POST", "/api/people/bulk", { ...input, confirm: true }),
    meta: { silentError: true },
    onSuccess: () => qc.invalidateQueries({ queryKey: peopleKeys.bulkRecent }),
  });
}

export function useBulkAction(id: string | null) {
  const refresh = useRefreshAll();
  const qc = useQueryClient();
  return useQuery({
    queryKey: peopleKeys.bulk(id ?? ""),
    queryFn: async () => {
      const b = await api<BulkAction>("GET", `/api/people/bulk/${id}`);
      if (b.status === "done" || b.status === "failed") {
        refresh();
        void qc.invalidateQueries({ queryKey: peopleKeys.bulkRecent, exact: true });
      }
      return b;
    },
    enabled: id !== null,
    // Poll while it runs.
    refetchInterval: (q) => (q.state.data && (q.state.data.status === "done" || q.state.data.status === "failed") ? false : 1500),
  });
}

export function useRecentBulk() {
  return useQuery({ queryKey: peopleKeys.bulkRecent, queryFn: () => api<{ data: BulkAction[] }>("GET", "/api/people/bulk?limit=20") });
}

export function usePrivacyRequests() {
  return useQuery({ queryKey: peopleKeys.requests, queryFn: () => api<{ data: PrivacyRequest[]; total: number }>("GET", "/api/people/privacy/requests?limit=100") });
}

export function useExportPerson() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (input: PersonInput) => api<ExportResult>("POST", "/api/people/privacy/export", input),
    onSuccess: () => qc.invalidateQueries({ queryKey: peopleKeys.requests }),
  });
}

export function useDeletePerson() {
  const refresh = useRefreshAll();
  return useMutation({
    mutationFn: (input: PersonInput) => api<DeleteResult>("POST", "/api/people/privacy/delete", input),
    onSuccess: (r) => {
      if (!r.dry_run) refresh();
    },
  });
}
