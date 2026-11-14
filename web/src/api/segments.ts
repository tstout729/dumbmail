import { keepPreviousData, useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { api } from "./client";
import { keys } from "./keys";
import type { ListResponse, Profile, Segment, SegmentDefinition } from "./types";

export const segmentsApi = {
  list: () => api<ListResponse<Segment>>("GET", "/api/segments"),
  get: (id: string) => api<Segment>("GET", `/api/segments/${id}`),
  create: (input: { name: string; definition: SegmentDefinition }) => api<Segment>("POST", "/api/segments", input),
  update: (id: string, input: { name?: string; definition?: SegmentDefinition }) =>
    api<Segment>("PATCH", `/api/segments/${id}`, input),
  remove: (id: string) => api<Segment>("DELETE", `/api/segments/${id}`),
  preview: (definition: SegmentDefinition) =>
    api<{ count: number; sample: Profile[] }>("POST", "/api/segments/preview", { definition }),
};

export function useSegments() {
  return useQuery({ queryKey: keys.segments, queryFn: segmentsApi.list, select: (r) => r.data });
}

export function useSegment(id: string) {
  return useQuery({ queryKey: keys.segment(id), queryFn: () => segmentsApi.get(id) });
}

/** Live preview; pass `null` while the definition is incomplete. */
export function useSegmentPreview(definition: SegmentDefinition | null) {
  return useQuery({
    queryKey: keys.segmentPreview(definition),
    queryFn: () => segmentsApi.preview(definition as SegmentDefinition),
    enabled: definition !== null,
    placeholderData: keepPreviousData,
    staleTime: 10_000,
  });
}

export function useCreateSegment() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: segmentsApi.create,
    onSuccess: (segment) => {
      qc.setQueryData(keys.segment(segment.id), segment);
      void qc.invalidateQueries({ queryKey: keys.segments });
    },
  });
}

export function useUpdateSegment(id: string) {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (input: { name?: string; definition?: SegmentDefinition }) => segmentsApi.update(id, input),
    onSuccess: (segment) => {
      qc.setQueryData(keys.segment(id), segment);
      void qc.invalidateQueries({ queryKey: keys.segments });
    },
  });
}

export function useDeleteSegment() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: segmentsApi.remove,
    onSuccess: () => qc.invalidateQueries({ queryKey: keys.segments }),
  });
}
