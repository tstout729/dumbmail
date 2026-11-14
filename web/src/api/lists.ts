import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { api } from "./client";
import { keys } from "./keys";
import type { List, ListResponse } from "./types";

export const listsApi = {
  list: () => api<ListResponse<List>>("GET", "/api/lists"),
  get: (id: string) => api<List>("GET", `/api/lists/${id}`),
  create: (input: { name: string; double_opt_in?: boolean }) => api<List>("POST", "/api/lists", input),
  update: (id: string, input: { name?: string }) => api<List>("PATCH", `/api/lists/${id}`, input),
  remove: (id: string) => api<List>("DELETE", `/api/lists/${id}`),
  addMembers: (id: string, input: { profile_ids?: string[]; emails?: string[] }) =>
    api<{ added: number }>("POST", `/api/lists/${id}/members`, input),
  removeMembers: (id: string, profile_ids: string[]) =>
    api<{ removed: number }>("DELETE", `/api/lists/${id}/members`, { profile_ids }),
};

export function useLists() {
  return useQuery({ queryKey: keys.lists, queryFn: listsApi.list, select: (r) => r.data });
}

export function useList(id: string) {
  return useQuery({ queryKey: keys.list(id), queryFn: () => listsApi.get(id) });
}

export function useCreateList() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: listsApi.create,
    onSuccess: () => qc.invalidateQueries({ queryKey: keys.lists }),
  });
}

export function useUpdateList(id: string) {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (input: { name?: string }) => listsApi.update(id, input),
    onSuccess: (list) => {
      qc.setQueryData(keys.list(id), list);
      void qc.invalidateQueries({ queryKey: keys.lists });
    },
  });
}

export function useDeleteList() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: listsApi.remove,
    onSuccess: () => qc.invalidateQueries({ queryKey: keys.lists }),
  });
}

export function useAddListMembers(id: string) {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (input: { profile_ids?: string[]; emails?: string[] }) => listsApi.addMembers(id, input),
    onSuccess: () => {
      void qc.invalidateQueries({ queryKey: keys.list(id) });
      void qc.invalidateQueries({ queryKey: keys.lists });
      void qc.invalidateQueries({ queryKey: keys.profilesAll });
    },
  });
}

export function useRemoveListMembers(id: string) {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (profileIds: string[]) => listsApi.removeMembers(id, profileIds),
    onSuccess: () => {
      void qc.invalidateQueries({ queryKey: keys.list(id) });
      void qc.invalidateQueries({ queryKey: keys.lists });
      void qc.invalidateQueries({ queryKey: keys.profilesAll });
    },
  });
}
