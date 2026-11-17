import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { api } from "./client";
import { keys } from "./keys";
import type { AutoRecharge, Billing } from "./types";

export const billingApi = {
  get: () => api<Billing>("GET", "/api/billing"),
  checkout: (amount: number) => api<{ url: string }>("POST", "/api/billing/checkout", { amount }),
  autoRecharge: (input: AutoRecharge) => api<Billing>("PATCH", "/api/billing/auto-recharge", input),
  choosePlan: (tier: number) => api<{ url: string | null; billing: Billing }>("POST", "/api/billing/plan", { tier }),
  cancelPlan: () => api<Billing>("POST", "/api/billing/plan/cancel"),
  resumePlan: () => api<Billing>("POST", "/api/billing/plan/resume"),
};

export function useBilling() {
  return useQuery({ queryKey: keys.billing, queryFn: billingApi.get });
}

/** Starts a Stripe Checkout top-up; the caller redirects to the returned URL. */
export function useCheckout() {
  return useMutation({ mutationFn: billingApi.checkout });
}

export function useAutoRecharge() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: billingApi.autoRecharge,
    onSuccess: (billing) => qc.setQueryData(keys.billing, billing),
  });
}

/** Start, upgrade or downgrade the email plan. A returned `url` is a Stripe Checkout page to redirect to. */
export function useChoosePlan() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: billingApi.choosePlan,
    onSuccess: ({ billing }) => qc.setQueryData(keys.billing, billing),
  });
}

export function useCancelPlan() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: billingApi.cancelPlan,
    onSuccess: (billing) => qc.setQueryData(keys.billing, billing),
  });
}

export function useResumePlan() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: billingApi.resumePlan,
    onSuccess: (billing) => qc.setQueryData(keys.billing, billing),
  });
}
