import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { api, withQuery } from "../../api/client";
import type { Delivery, DeliveryDetail, DeliveryStatus, FromRecipeResult, Integration, IntegrationChanges, IntegrationStatus, IntegrationSummary, Recipe, TestResult } from "./types";

const keys = {
  all: ["integrations"] as const,
  one: (id: string) => ["integrations", id] as const,
  deliveries: (id: string, status: string) => ["integrations", id, "deliveries", status] as const,
  delivery: (id: string, deliveryId: string) => ["integrations", id, "delivery", deliveryId] as const,
  recipes: ["integration-recipes"] as const,
};

export type TestInput = { script?: string; payload?: unknown; headers?: Record<string, string>; pull?: boolean };

export const integrationsApi = {
  list: () => api<{ data: IntegrationSummary[] }>("GET", "/api/integrations"),
  get: (id: string) => api<Integration>("GET", `/api/integrations/${id}`),
  create: (input: IntegrationChanges & { name: string }) => api<Integration>("POST", "/api/integrations", input),
  update: (id: string, changes: IntegrationChanges) => api<Integration>("PATCH", `/api/integrations/${id}`, changes),
  remove: (id: string) => api<{ ok: boolean }>("DELETE", `/api/integrations/${id}`),
  setStatus: (id: string, status: IntegrationStatus) => api<Integration>("POST", `/api/integrations/${id}/status`, { status }),
  rotateUrl: (id: string) => api<Integration>("POST", `/api/integrations/${id}/rotate-url`),
  test: (id: string, input: TestInput) => api<TestResult>("POST", `/api/integrations/${id}/test`, input),
  pull: (id: string, backfillSince?: string) => api<Record<string, unknown>>("POST", `/api/integrations/${id}/pull`, backfillSince ? { backfill_since: backfillSince } : {}),
  resumeBackfill: (id: string) => api<Record<string, unknown>>("POST", `/api/integrations/${id}/pull`, { resume_backfill: true }),
  cancelBackfill: (id: string) => api<{ cancelled: boolean }>("POST", `/api/integrations/${id}/backfill/cancel`),
  deliveries: (id: string, status: DeliveryStatus | "") => api<{ data: Delivery[]; total: number }>("GET", withQuery(`/api/integrations/${id}/deliveries`, { status, limit: 100 })),
  delivery: (id: string, deliveryId: string) => api<DeliveryDetail>("GET", `/api/integrations/${id}/deliveries/${deliveryId}`),
  replay: (id: string, input: { delivery_ids?: string[]; all_failed?: boolean }) => api<{ replayed: number }>("POST", `/api/integrations/${id}/replay`, input),
  recipes: () => api<{ data: Recipe[] }>("GET", "/api/integration-recipes"),
  fromRecipe: (input: { recipe: string; name?: string; create_flows?: boolean }) => api<FromRecipeResult>("POST", "/api/integrations/from-recipe", input),
};

export function useIntegrations() {
  return useQuery({ queryKey: keys.all, queryFn: integrationsApi.list, select: (r) => r.data });
}

export function useIntegration(id: string) {
  // A running backfill reports progress; refresh while it runs.
  return useQuery({ queryKey: keys.one(id), queryFn: () => integrationsApi.get(id), refetchInterval: (q) => (q.state.data?.backfill?.status === "running" ? 3000 : false) });
}

export function useRecipes() {
  return useQuery({ queryKey: keys.recipes, queryFn: integrationsApi.recipes, select: (r) => r.data, staleTime: 60 * 60_000 });
}

export function useDeliveries(id: string, status: DeliveryStatus | "") {
  return useQuery({ queryKey: keys.deliveries(id, status), queryFn: () => integrationsApi.deliveries(id, status), refetchInterval: 10_000 });
}

export function useDelivery(id: string, deliveryId: string | null) {
  return useQuery({ queryKey: keys.delivery(id, deliveryId ?? ""), queryFn: () => integrationsApi.delivery(id, deliveryId ?? ""), enabled: deliveryId !== null });
}

function useWriter<V>(fn: (v: V) => Promise<Integration>) {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: fn,
    onSuccess: (i) => {
      qc.setQueryData(keys.one(i.id), i);
      void qc.invalidateQueries({ queryKey: keys.all, exact: true });
    },
  });
}

export function useCreateIntegration() {
  return useWriter(integrationsApi.create);
}

export function useUpdateIntegration(id: string) {
  return useWriter((c: IntegrationChanges) => integrationsApi.update(id, c));
}

export function useSetIntegrationStatus(id: string) {
  return useWriter((s: IntegrationStatus) => integrationsApi.setStatus(id, s));
}

export function useRotateIntegrationUrl(id: string) {
  return useWriter(() => integrationsApi.rotateUrl(id));
}

export function useTestIntegration(id: string) {
  return useMutation({ mutationFn: (input: TestInput) => integrationsApi.test(id, input) });
}

function useRefreshing<V, R>(id: string, fn: (v: V) => Promise<R>) {
  const qc = useQueryClient();
  return useMutation({ mutationFn: fn, onSuccess: () => void qc.invalidateQueries({ queryKey: keys.one(id) }) });
}

export function useRunPull(id: string) {
  return useRefreshing(id, (since: string | undefined) => integrationsApi.pull(id, since));
}

export function useResumeBackfill(id: string) {
  return useRefreshing(id, () => integrationsApi.resumeBackfill(id));
}

export function useCancelBackfill(id: string) {
  return useRefreshing(id, () => integrationsApi.cancelBackfill(id));
}

export function useReplay(id: string) {
  return useRefreshing(id, (input: { delivery_ids?: string[]; all_failed?: boolean }) => integrationsApi.replay(id, input));
}

export function useDeleteIntegration() {
  const qc = useQueryClient();
  return useMutation({ mutationFn: integrationsApi.remove, onSuccess: () => void qc.invalidateQueries({ queryKey: keys.all }) });
}

export function useCreateFromRecipe() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: integrationsApi.fromRecipe,
    onSuccess: (r) => {
      qc.setQueryData(keys.one(r.integration.id), r.integration);
      void qc.invalidateQueries({ queryKey: keys.all, exact: true });
      void qc.invalidateQueries({ queryKey: ["flows"] });
    },
  });
}
