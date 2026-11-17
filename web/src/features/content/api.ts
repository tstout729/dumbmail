import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { api, withQuery } from "../../api/client";
import { keys } from "../../api/keys";
import type { EmailBlock, Template } from "../../api/types";
import type { CheckInput, CheckResult, SavedBlock, SavedBlockDetail, Starter, StarterDetail } from "./types";

export const contentKeys = {
  saved: ["content", "saved-blocks"] as const,
  savedOne: (id: string) => ["content", "saved-blocks", id] as const,
  starters: (preview: boolean) => ["content", "starters", preview] as const,
  starter: (key: string) => ["content", "starter", key] as const,
};

export type SavedBlockInput = { name?: string; blocks?: EmailBlock[] };

export const contentApi = {
  savedBlocks: () => api<{ data: SavedBlock[]; total: number }>("GET", "/api/content/saved-blocks"),
  savedBlock: (id: string) => api<SavedBlockDetail>("GET", `/api/content/saved-blocks/${id}`),
  /** The content an email using this saved block shows now (also after it was deleted). */
  savedBlockShown: (id: string) => api<SavedBlockDetail & { deleted?: boolean }>("GET", withQuery(`/api/content/saved-blocks/${id}`, { include_deleted: true })),
  createSaved: (input: SavedBlockInput) => api<SavedBlock>("POST", "/api/content/saved-blocks", input),
  updateSaved: (id: string, input: SavedBlockInput) => api<SavedBlockDetail>("PATCH", `/api/content/saved-blocks/${id}`, input),
  removeSaved: (id: string) => api<{ ok: true }>("DELETE", `/api/content/saved-blocks/${id}`),
  starters: (preview: boolean) => api<{ data: Starter[] }>("GET", withQuery("/api/content/starters", { preview: preview || undefined })),
  starter: (key: string) => api<StarterDetail>("GET", `/api/content/starters/${encodeURIComponent(key)}`),
  templateFromStarter: (key: string, name?: string) => api<Template>("POST", `/api/content/starters/${encodeURIComponent(key)}/template`, { name }),
  check: (input: CheckInput) => api<CheckResult>("POST", "/api/content/check", input),
  videoThumbnail: (input: { video_url?: string; image_url?: string }) =>
    api<{ thumbnail_url: string; source: string }>("POST", "/api/content/video-thumbnail", input),
};

export function useSavedBlocks() {
  return useQuery({ queryKey: contentKeys.saved, queryFn: contentApi.savedBlocks, select: (r) => r.data });
}

export function useSavedBlock(id: string) {
  return useQuery({ queryKey: contentKeys.savedOne(id), queryFn: () => contentApi.savedBlock(id) });
}

/** After a saved block changes, every preview that shows it is out of date. */
function invalidateSaved(qc: ReturnType<typeof useQueryClient>) {
  void qc.invalidateQueries({ queryKey: contentKeys.saved });
  void qc.invalidateQueries({ queryKey: ["render"] });
}

export function useCreateSavedBlock() {
  const qc = useQueryClient();
  return useMutation({ mutationFn: contentApi.createSaved, onSuccess: () => invalidateSaved(qc) });
}

export function useUpdateSavedBlock(id: string) {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (input: SavedBlockInput) => contentApi.updateSaved(id, input),
    onSuccess: (b) => {
      qc.setQueryData(contentKeys.savedOne(id), b);
      invalidateSaved(qc);
    },
  });
}

export function useDeleteSavedBlock() {
  const qc = useQueryClient();
  return useMutation({ mutationFn: contentApi.removeSaved, onSuccess: () => invalidateSaved(qc) });
}

export function useStarters(preview: boolean, enabled = true) {
  return useQuery({ queryKey: contentKeys.starters(preview), queryFn: () => contentApi.starters(preview), select: (r) => r.data, staleTime: 5 * 60_000, enabled });
}

export function useTemplateFromStarter() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: ({ key, name }: { key: string; name?: string }) => contentApi.templateFromStarter(key, name),
    onSuccess: () => qc.invalidateQueries({ queryKey: keys.templatesAll }),
  });
}

export function useCheckEmail() {
  return useMutation({ mutationFn: contentApi.check });
}

export function useVideoThumbnail() {
  return useMutation({ mutationFn: contentApi.videoThumbnail });
}
