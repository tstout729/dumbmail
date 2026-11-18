import { keepPreviousData, useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { api, withQuery } from "../../api/client";
import type { ListResponse } from "../../api/types";
import { downloadCsv } from "../reports/api";
import type {
  ImportInput,
  ImportPreview,
  ImportRun,
  ModerateInput,
  ModerateResult,
  Review,
  ReviewList,
  ReviewProduct,
  ReviewQuery,
  ReviewReport,
  ReviewSettings,
  ReviewSettingsPatch,
} from "./types";

export const reviewKeys = {
  all: ["reviews"] as const,
  list: (q: ReviewQuery) => ["reviews", "list", q] as const,
  settings: ["reviews", "settings"] as const,
  products: (q: string) => ["reviews", "products", q] as const,
  report: (days: number) => ["reviews", "report", days] as const,
  import: (id: string) => ["reviews", "import", id] as const,
};

type Query = Record<string, string | number | boolean | undefined>;

export const reviewsApi = {
  list: (q: ReviewQuery) => api<ReviewList>("GET", withQuery("/api/reviews", q as Query)),
  moderate: (input: ModerateInput) => api<ModerateResult>("POST", "/api/reviews/moderate", input),
  reply: (id: string, text: string) => api<Review>("POST", `/api/reviews/${encodeURIComponent(id)}/reply`, { text }),
  removePhoto: (id: string, photoId: string) => api<{ ok: true }>("DELETE", `/api/reviews/${encodeURIComponent(id)}/photos/${encodeURIComponent(photoId)}`),
  remove: (id: string) => api<{ ok: true }>("DELETE", withQuery(`/api/reviews/${encodeURIComponent(id)}`, { confirm: true })),
  settings: () => api<ReviewSettings>("GET", "/api/reviews/settings"),
  updateSettings: (p: ReviewSettingsPatch) => api<ReviewSettings>("PATCH", "/api/reviews/settings", p),
  products: (q: string) => api<ListResponse<ReviewProduct>>("GET", withQuery("/api/reviews/products", { q, limit: 100 })),
  rebuild: () => api<{ ok: true; note: string }>("POST", "/api/reviews/products/rebuild"),
  report: (days: number) => api<ReviewReport>("GET", withQuery("/api/reviews/report", { days })),
  importCsv: (input: ImportInput) => api<ImportPreview | ImportRun>("POST", "/api/reviews/import", input),
  getImport: (id: string) => api<ImportRun>("GET", `/api/reviews/imports/${encodeURIComponent(id)}`),
  createFlow: (kind: "order" | "stay") => api<{ id: string; name: string; note: string }>("POST", "/api/reviews/flows", { kind }),
};

export function exportReviews(q: ReviewQuery) {
  return downloadCsv(withQuery("/api/reviews/export", q as Query), undefined, "reviews.csv");
}

export function useReviews(q: ReviewQuery) {
  return useQuery({ queryKey: reviewKeys.list(q), queryFn: () => reviewsApi.list(q), placeholderData: keepPreviousData });
}

export function useReviewSettings() {
  return useQuery({ queryKey: reviewKeys.settings, queryFn: reviewsApi.settings });
}

export function useReviewProducts(q: string) {
  return useQuery({ queryKey: reviewKeys.products(q), queryFn: () => reviewsApi.products(q), placeholderData: keepPreviousData });
}

export function useReviewReport(days: number) {
  return useQuery({ queryKey: reviewKeys.report(days), queryFn: () => reviewsApi.report(days) });
}

export function useImportRun(id: string | null) {
  return useQuery({
    queryKey: reviewKeys.import(id ?? ""),
    queryFn: () => reviewsApi.getImport(id ?? ""),
    enabled: id !== null,
    refetchInterval: (q) => (q.state.data && (q.state.data.status === "done" || q.state.data.status === "failed") ? false : 2000),
  });
}

function useInvalidate() {
  const qc = useQueryClient();
  return () => qc.invalidateQueries({ queryKey: reviewKeys.all });
}

export function useModerate() {
  const invalidate = useInvalidate();
  return useMutation({ mutationFn: reviewsApi.moderate, onSuccess: invalidate });
}

export function useReply() {
  const invalidate = useInvalidate();
  return useMutation({ mutationFn: ({ id, text }: { id: string; text: string }) => reviewsApi.reply(id, text), onSuccess: invalidate });
}

export function useRemovePhoto() {
  const invalidate = useInvalidate();
  return useMutation({ mutationFn: ({ id, photoId }: { id: string; photoId: string }) => reviewsApi.removePhoto(id, photoId), onSuccess: invalidate });
}

export function useDeleteReview() {
  const invalidate = useInvalidate();
  return useMutation({ mutationFn: reviewsApi.remove, onSuccess: invalidate });
}

export function useUpdateReviewSettings() {
  const invalidate = useInvalidate();
  return useMutation({ mutationFn: reviewsApi.updateSettings, onSuccess: invalidate });
}

export function useRebuild() {
  const invalidate = useInvalidate();
  return useMutation({ mutationFn: reviewsApi.rebuild, onSuccess: invalidate });
}

export function useImportReviews() {
  const invalidate = useInvalidate();
  return useMutation({ mutationFn: reviewsApi.importCsv, onSuccess: invalidate });
}

export function useCreateReviewFlow() {
  return useMutation({ mutationFn: reviewsApi.createFlow });
}
