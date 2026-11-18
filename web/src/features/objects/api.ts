import { keepPreviousData, useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { api, withQuery } from "../../api/client";
import type { ObjectDetail, ObjectPage, ObjectType, ObjectTypeInput, ProfileObjects } from "./types";

const enc = encodeURIComponent;

export const objectKeys = {
  all: ["objects"] as const,
  types: ["objects", "types"] as const,
  type: (key: string) => ["objects", "type", key] as const,
  list: (key: string, q: string, offset: number) => ["objects", "list", key, q, offset] as const,
  object: (id: string) => ["objects", "object", id] as const,
  profile: (id: string) => ["objects", "profile", id] as const,
};

export const objectsApi = {
  types: () => api<{ data: ObjectType[] }>("GET", "/api/object-types"),
  type: (key: string) => api<ObjectType>("GET", `/api/object-types/${enc(key)}`),
  saveType: (input: ObjectTypeInput) => api<ObjectType>("POST", "/api/object-types", input),
  deleteType: (key: string) => api<{ deleted: boolean; objects: number }>("DELETE", withQuery(`/api/object-types/${enc(key)}`, { confirm: true })),
  list: (key: string, q: string, offset: number) => api<ObjectPage>("GET", withQuery(`/api/object-types/${enc(key)}/objects`, { q: q || undefined, limit: 50, offset })),
  get: (id: string) => api<ObjectDetail>("GET", `/api/objects/${enc(id)}`),
  remove: (id: string) => api<null>("DELETE", `/api/objects/${enc(id)}`),
  forProfile: (id: string) => api<ProfileObjects>("GET", `/api/profiles/${enc(id)}/objects`),
};

export function useObjectTypes() {
  return useQuery({ queryKey: objectKeys.types, queryFn: objectsApi.types });
}

export function useObjectType(key: string) {
  return useQuery({ queryKey: objectKeys.type(key), queryFn: () => objectsApi.type(key), enabled: key !== "" });
}

export function useObjects(key: string, q: string, offset: number) {
  return useQuery({ queryKey: objectKeys.list(key, q, offset), queryFn: () => objectsApi.list(key, q, offset), placeholderData: keepPreviousData, enabled: key !== "" });
}

export function useObject(id: string) {
  return useQuery({ queryKey: objectKeys.object(id), queryFn: () => objectsApi.get(id), enabled: id !== "" });
}

export function useProfileObjects(id: string, enabled: boolean) {
  return useQuery({ queryKey: objectKeys.profile(id), queryFn: () => objectsApi.forProfile(id), enabled: enabled && id !== "" });
}

function useInvalidate() {
  const qc = useQueryClient();
  return () => qc.invalidateQueries({ queryKey: objectKeys.all });
}

export function useSaveObjectType() {
  const invalidate = useInvalidate();
  return useMutation({ mutationFn: objectsApi.saveType, onSuccess: invalidate });
}

export function useDeleteObjectType() {
  const invalidate = useInvalidate();
  return useMutation({ mutationFn: objectsApi.deleteType, onSuccess: invalidate });
}

export function useDeleteObject() {
  const invalidate = useInvalidate();
  return useMutation({ mutationFn: objectsApi.remove, onSuccess: invalidate });
}
