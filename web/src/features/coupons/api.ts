import { keepPreviousData, useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { ApiError, api, withQuery } from "../../api/client";
import type { ListResponse } from "../../api/types";
import type { AddCodesResult, Coupon, CouponCampaignCheck, CouponCodePage, CouponCodeQuery, CouponInput, CouponPattern, CouponReport, ExportFormat, GenerateResult } from "./types";

export const couponKeys = {
  all: ["coupons"] as const,
  list: ["coupons", "list"] as const,
  coupon: (id: string) => ["coupons", "coupon", id] as const,
  codes: (id: string, q: CouponCodeQuery) => ["coupons", "codes", id, q] as const,
  report: (id: string) => ["coupons", "report", id] as const,
  campaignCheck: (campaignId: string) => ["coupons", "campaign-check", campaignId] as const,
};

/** Send a CSV or text file as itself: the server reads it as it arrives (up to 16 MB). */
async function uploadFile(id: string, file: File): Promise<AddCodesResult> {
  let res: Response;
  try {
    res = await fetch(`/api/coupons/${encodeURIComponent(id)}/codes`, {
      method: "POST",
      credentials: "include",
      headers: { "Content-Type": file.name.toLowerCase().endsWith(".txt") ? "text/plain" : "text/csv", Accept: "application/json" },
      body: file,
    });
  } catch {
    throw new ApiError(0, "network", "Can't reach the DumbMail server. Check your connection and try again.");
  }
  const json: unknown = await res.json().catch(() => null);
  if (!res.ok) {
    const err = (json as { error?: { message?: string } } | null)?.error;
    throw new ApiError(res.status, res.status === 409 ? "conflict" : "bad_request", err?.message ?? `Upload failed (${res.status})`);
  }
  return json as AddCodesResult;
}

export const couponsApi = {
  list: () => api<ListResponse<Coupon>>("GET", withQuery("/api/coupons", { limit: 100 })),
  get: (id: string) => api<Coupon>("GET", `/api/coupons/${encodeURIComponent(id)}`),
  create: (input: CouponInput) => api<Coupon>("POST", "/api/coupons", input),
  update: (id: string, input: CouponInput) => api<Coupon>("PATCH", `/api/coupons/${encodeURIComponent(id)}`, input),
  remove: (id: string) => api<null>("DELETE", withQuery(`/api/coupons/${encodeURIComponent(id)}`, { confirm: true })),
  codes: (id: string, q: CouponCodeQuery) => api<CouponCodePage>("GET", withQuery(`/api/coupons/${encodeURIComponent(id)}/codes`, q)),
  addCodes: (id: string, input: string | File) => (typeof input === "string" ? api<AddCodesResult>("POST", `/api/coupons/${encodeURIComponent(id)}/codes`, { text: input }) : uploadFile(id, input)),
  generate: (id: string, count: number, pattern?: CouponPattern) => api<GenerateResult>("POST", `/api/coupons/${encodeURIComponent(id)}/generate`, pattern ? { count, pattern } : { count }),
  report: (id: string) => api<CouponReport>("GET", withQuery(`/api/coupons/${encodeURIComponent(id)}/report`, { days: 30 })),
  campaignCheck: (campaignId: string) => api<CouponCampaignCheck>("GET", `/api/coupons/campaign-check/${encodeURIComponent(campaignId)}`),
};

export const exportUrl = (id: string, status: "available" | "all", format: ExportFormat) => withQuery(`/api/coupons/${encodeURIComponent(id)}/export`, { status, format });

export function useCoupons() {
  return useQuery({ queryKey: couponKeys.list, queryFn: couponsApi.list });
}

export function useCoupon(id: string) {
  // While codes are being generated in the background, check back for progress.
  return useQuery({ queryKey: couponKeys.coupon(id), queryFn: () => couponsApi.get(id), enabled: id !== "", refetchInterval: (q) => ((q.state.data?.stock.generating ?? 0) > 0 ? 3000 : false) });
}

export function useCouponCodes(id: string, q: CouponCodeQuery) {
  return useQuery({ queryKey: couponKeys.codes(id, q), queryFn: () => couponsApi.codes(id, q), placeholderData: keepPreviousData });
}

export function useCouponCampaignCheck(campaignId: string) {
  return useQuery({ queryKey: couponKeys.campaignCheck(campaignId), queryFn: () => couponsApi.campaignCheck(campaignId), staleTime: 0 });
}

export function useCouponReport(id: string) {
  return useQuery({ queryKey: couponKeys.report(id), queryFn: () => couponsApi.report(id) });
}

function useInvalidate() {
  const qc = useQueryClient();
  return () => qc.invalidateQueries({ queryKey: couponKeys.all });
}

export function useCreateCoupon() {
  const invalidate = useInvalidate();
  return useMutation({ mutationFn: couponsApi.create, onSuccess: invalidate });
}

export function useUpdateCoupon(id: string) {
  const invalidate = useInvalidate();
  return useMutation({ mutationFn: (input: CouponInput) => couponsApi.update(id, input), onSuccess: invalidate });
}

export function useDeleteCoupon() {
  const invalidate = useInvalidate();
  return useMutation({ mutationFn: couponsApi.remove, onSuccess: invalidate });
}

export function useAddCodes(id: string) {
  const invalidate = useInvalidate();
  return useMutation({ mutationFn: (input: string | File) => couponsApi.addCodes(id, input), onSuccess: invalidate });
}

export function useGenerateCodes(id: string) {
  const invalidate = useInvalidate();
  return useMutation({ mutationFn: ({ count, pattern }: { count: number; pattern?: CouponPattern }) => couponsApi.generate(id, count, pattern), onSuccess: invalidate });
}
