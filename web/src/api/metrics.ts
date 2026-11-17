import { keepPreviousData, useQuery } from "@tanstack/react-query";
import { api, withQuery } from "./client";
import { keys } from "./keys";
import type { AnalyticsOverview, FlowAnalytics, EventRow, ListResponse, MetricQuery, MetricQueryResult, MetricSummary } from "./types";

export type EventQuery = { metric?: string; profile_id?: string; limit?: number; offset?: number };

export const metricsApi = {
  list: () => api<{ data: MetricSummary[] }>("GET", "/api/metrics"),
  query: (q: MetricQuery) => api<MetricQueryResult>("POST", "/api/metrics/query", q),
  events: (params: EventQuery) => api<ListResponse<EventRow>>("GET", withQuery("/api/events", params)),
  overview: (days: number) => api<AnalyticsOverview>("GET", withQuery("/api/analytics/overview", { days })),
  flows: (days: number) => api<FlowAnalytics>("GET", withQuery("/api/analytics/flows", { days })),
};

export function useMetrics() {
  return useQuery({ queryKey: keys.metrics, queryFn: metricsApi.list, select: (r) => r.data });
}

export function useMetricQuery(q: MetricQuery | null) {
  return useQuery({
    queryKey: keys.metricQuery(q),
    queryFn: () => metricsApi.query(q as MetricQuery),
    enabled: q !== null && q.metric !== "",
    placeholderData: keepPreviousData,
  });
}

export function useEvents(params: EventQuery) {
  return useQuery({
    queryKey: keys.events(params),
    queryFn: () => metricsApi.events(params),
    placeholderData: keepPreviousData,
  });
}

export function useOverview(days: number) {
  return useQuery({
    queryKey: keys.overview(days),
    queryFn: () => metricsApi.overview(days),
    placeholderData: keepPreviousData,
  });
}

export function useFlowAnalytics(days: number) {
  return useQuery({
    queryKey: keys.flowAnalytics(days),
    queryFn: () => metricsApi.flows(days),
    placeholderData: keepPreviousData,
  });
}
