import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { api, withQuery } from "../../api/client";
import type { CreatedEndpoint, DeliveryDetail, DeliveryStatus, Delivery, Endpoint, EndpointInput, FlowSecret, RotatedSecret, TestResult, TopicInfo } from "./types";

const keys = {
  all: ["webhooks"] as const,
  topics: ["webhooks", "topics"] as const,
  deliveries: (id: string, status: string) => ["webhooks", id, "deliveries", status] as const,
  delivery: (id: string, deliveryId: string) => ["webhooks", id, "delivery", deliveryId] as const,
  flowSecret: ["webhooks", "flow-secret"] as const,
};

export const webhooksApi = {
  topics: () => api<{ data: TopicInfo[] }>("GET", "/api/webhooks/topics"),
  list: () => api<{ data: Endpoint[] }>("GET", "/api/webhooks"),
  create: (input: EndpointInput) => api<CreatedEndpoint>("POST", "/api/webhooks", input),
  update: (id: string, input: EndpointInput) => api<Endpoint>("PATCH", `/api/webhooks/${id}`, input),
  remove: (id: string) => api<{ ok: boolean }>("DELETE", `/api/webhooks/${id}`),
  rotate: (id: string) => api<RotatedSecret>("POST", `/api/webhooks/${id}/rotate-secret`),
  test: (id: string, topic?: string) => api<TestResult>("POST", `/api/webhooks/${id}/test`, topic ? { topic } : {}),
  deliveries: (id: string, status: DeliveryStatus | "") => api<{ data: Delivery[]; total: number }>("GET", withQuery(`/api/webhooks/${id}/deliveries`, { status, limit: 100 })),
  delivery: (id: string, deliveryId: string) => api<DeliveryDetail>("GET", `/api/webhooks/${id}/deliveries/${deliveryId}`),
  resend: (id: string, input: { delivery_ids?: string[]; since?: string; status?: "failed" | "delivered" | "all" }) => api<{ resent: number }>("POST", `/api/webhooks/${id}/resend`, input),
  flowSecret: () => api<FlowSecret>("GET", "/api/webhooks/flow-secret"),
  rotateFlowSecret: () => api<RotatedSecret>("POST", "/api/webhooks/flow-secret/rotate"),
};

export function useTopics() {
  return useQuery({ queryKey: keys.topics, queryFn: webhooksApi.topics, select: (r) => r.data, staleTime: 60 * 60_000 });
}

export function useEndpoints() {
  return useQuery({ queryKey: keys.all, queryFn: webhooksApi.list, select: (r) => r.data, refetchInterval: 30_000 });
}

export function useDeliveries(id: string, status: DeliveryStatus | "") {
  return useQuery({ queryKey: keys.deliveries(id, status), queryFn: () => webhooksApi.deliveries(id, status), refetchInterval: 10_000 });
}

export function useDelivery(id: string, deliveryId: string | null) {
  return useQuery({ queryKey: keys.delivery(id, deliveryId ?? ""), queryFn: () => webhooksApi.delivery(id, deliveryId ?? ""), enabled: deliveryId !== null });
}

export function useFlowSecret() {
  return useQuery({ queryKey: keys.flowSecret, queryFn: webhooksApi.flowSecret });
}

function useInvalidating<V, R>(fn: (v: V) => Promise<R>, silent = false) {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: fn,
    meta: { silentError: silent },
    onSuccess: () => qc.invalidateQueries({ queryKey: keys.all }),
  });
}

export function useCreateEndpoint() {
  return useInvalidating(webhooksApi.create, true);
}

export function useUpdateEndpoint() {
  return useInvalidating(({ id, input }: { id: string; input: EndpointInput }) => webhooksApi.update(id, input), true);
}

export function useDeleteEndpoint() {
  return useInvalidating(webhooksApi.remove);
}

export function useRotateSecret() {
  return useInvalidating(webhooksApi.rotate);
}

export function useSendTest() {
  return useInvalidating(({ id, topic }: { id: string; topic?: string }) => webhooksApi.test(id, topic));
}

export function useResend() {
  return useInvalidating(({ id, ...input }: { id: string; delivery_ids?: string[]; since?: string; status?: "failed" | "delivered" | "all" }) => webhooksApi.resend(id, input));
}

export function useRotateFlowSecret() {
  return useInvalidating(webhooksApi.rotateFlowSecret);
}
