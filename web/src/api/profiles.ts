import { keepPreviousData, useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { api, withQuery } from "./client";
import { keys } from "./keys";
import type { Consent, ImportResult, ListResponse, Profile, ProfileDetail, ProfileInput } from "./types";

export type ProfileQuery = { q?: string; list_id?: string; segment_id?: string; limit?: number; offset?: number };
export type ImportInput = { csv: string; list_id?: string; email_consent?: Consent; sms_consent?: Consent };

export const profilesApi = {
  list: (params: ProfileQuery) => api<ListResponse<Profile>>("GET", withQuery("/api/profiles", params)),
  get: (id: string) => api<ProfileDetail>("GET", `/api/profiles/${id}`),
  upsert: (input: ProfileInput) => api<Profile>("POST", "/api/profiles", input),
  update: (id: string, patch: ProfileInput) => api<Profile>("PATCH", `/api/profiles/${id}`, patch),
  remove: (id: string) => api<{ ok: true }>("DELETE", `/api/profiles/${id}`),
  import: (input: ImportInput) => api<ImportResult>("POST", "/api/profiles/import", input),
};

export function useProfiles(params: ProfileQuery) {
  return useQuery({
    queryKey: keys.profiles(params),
    queryFn: () => profilesApi.list(params),
    placeholderData: keepPreviousData,
  });
}

export function useProfile(id: string) {
  return useQuery({ queryKey: keys.profile(id), queryFn: () => profilesApi.get(id) });
}

export function useUpsertProfile() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: profilesApi.upsert,
    onSuccess: () => {
      void qc.invalidateQueries({ queryKey: keys.profilesAll });
      void qc.invalidateQueries({ queryKey: keys.lists });
    },
  });
}

export function useUpdateProfile(id: string) {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (patch: ProfileInput) => profilesApi.update(id, patch),
    onMutate: async (patch) => {
      await qc.cancelQueries({ queryKey: keys.profile(id) });
      const previous = qc.getQueryData<ProfileDetail>(keys.profile(id));
      if (previous) {
        const { list_ids: _ignored, ...fields } = patch;
        qc.setQueryData<ProfileDetail>(keys.profile(id), { ...previous, profile: { ...previous.profile, ...fields } });
      }
      return { previous };
    },
    onError: (_e, _p, ctx) => {
      if (ctx?.previous) qc.setQueryData(keys.profile(id), ctx.previous);
    },
    onSettled: () => {
      void qc.invalidateQueries({ queryKey: keys.profile(id) });
      void qc.invalidateQueries({ queryKey: keys.profilesAll });
    },
  });
}

export function useDeleteProfile() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: profilesApi.remove,
    onSuccess: () => qc.invalidateQueries({ queryKey: keys.profilesAll }),
  });
}

export function useImportProfiles() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: profilesApi.import,
    meta: { silentError: true },
    onSuccess: () => {
      void qc.invalidateQueries({ queryKey: keys.profilesAll });
      void qc.invalidateQueries({ queryKey: keys.lists });
    },
  });
}
