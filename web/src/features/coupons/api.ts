import { keepPreviousData, useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { api, withQuery } from "../../api/client";
import type { ListResponse } from "../../api/types";
import type { AddCodesResult, Coupon, CouponCodePage, CouponCodeQuery, CouponInput, CouponPattern, CouponReport, GenerateResult } from "./types";

export const couponKeys = {
  all: ["coupons"] as const,
  list: ["coupons", "list"] as const,
  coupon: (id: string) => ["coupons", "coupon", id] as const,
  codes: (id: string, q: CouponCodeQuery) => ["coupons", "codes", id, q] as const,
  report: (id: string) => ["coupons", "report", id] as const,
};

export const couponsApi = {
  list: () => api<ListResponse<Coupon>>("GET", withQuery("/api/coupons", { limit: 100 })),
  get: (id: string) => api<Coupon>("GET", `/api/coupons/${encodeURIComponent(id)}`),
  create: (input: CouponInput) => api<Coupon>("POST", "/api/coupons", input),
  update: (id: string, input: CouponInput) => api<Coupon>("PATCH", `/api/coupons/${encodeURIComponent(id)}`, input),
  remove: (id: string) => api<null>("DELETE", `/api/coupons/${encodeURIComponent(id)}`),
  codes: (id: string, q: CouponCodeQuery) => api<CouponCodePage>("GET", withQuery(`/api/coupons/${encodeURIComponent(id)}/codes`, q)),
  addCodes: (id: string, text: string) => api<AddCodesResult>("POST", `/api/coupons/${encodeURIComponent(id)}/codes`, { text }),
  generate: (id: string, count: number, pattern?: CouponPattern) => api<GenerateResult>("POST", `/api/coupons/${encodeURIComponent(id)}/generate`, pattern ? { count, pattern } : { count }),
  report: (id: string) => api<CouponReport>("GET", withQuery(`/api/coupons/${encodeURIComponent(id)}/report`, { days: 30 })),
};

export const exportUrl = (id: string, status: "available" | "all") => withQuery(`/api/coupons/${encodeURIComponent(id)}/export`, { status });

export function useCoupons() {
  return useQuery({ queryKey: couponKeys.list, queryFn: couponsApi.list });
}

export function useCoupon(id: string) {
  return useQuery({ queryKey: couponKeys.coupon(id), queryFn: () => couponsApi.get(id), enabled: id !== "" });
}

export function useCouponCodes(id: string, q: CouponCodeQuery) {
  return useQuery({ queryKey: couponKeys.codes(id, q), queryFn: () => couponsApi.codes(id, q), placeholderData: keepPreviousData });
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
  return useMutation({ mutationFn: (text: string) => couponsApi.addCodes(id, text), onSuccess: invalidate });
}

export function useGenerateCodes(id: string) {
  const invalidate = useInvalidate();
  return useMutation({ mutationFn: ({ count, pattern }: { count: number; pattern?: CouponPattern }) => couponsApi.generate(id, count, pattern), onSuccess: invalidate });
}
