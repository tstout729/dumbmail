import { keepPreviousData, useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { api, withQuery } from "../../api/client";
import type { ListResponse } from "../../api/types";
import type {
  CatalogFacets,
  CatalogSettings,
  CatalogSettingsPatch,
  CatalogSummary,
  Feed,
  FeedInput,
  Product,
  ProductDetail,
  ProductQuery,
  ProductRecommendations,
  UpsertSummary,
} from "./types";

export const catalogKeys = {
  all: ["catalog"] as const,
  products: (q: ProductQuery) => ["catalog", "products", q] as const,
  product: (key: string) => ["catalog", "product", key] as const,
  summary: ["catalog", "summary"] as const,
  facets: ["catalog", "facets"] as const,
  feeds: ["catalog", "feeds"] as const,
  settings: ["catalog", "settings"] as const,
  related: (key: string) => ["catalog", "related", key] as const,
};

export const catalogApi = {
  products: (q: ProductQuery) => api<ListResponse<Product>>("GET", withQuery("/api/catalog/products", q)),
  product: (key: string) => api<ProductDetail>("GET", `/api/catalog/products/${encodeURIComponent(key)}`),
  remove: (id: string) => api<{ ok: true }>("DELETE", `/api/catalog/products/${encodeURIComponent(id)}`),
  importCsv: (csv: string) => api<UpsertSummary>("POST", "/api/catalog/products/import", { csv }),
  summary: () => api<CatalogSummary>("GET", "/api/catalog/summary"),
  facets: () => api<CatalogFacets>("GET", "/api/catalog/facets"),
  related: (key: string) => api<ProductRecommendations>("GET", withQuery("/api/catalog/recommendations", { product_id: key, limit: 6 })),
  feeds: () => api<{ data: Feed[] }>("GET", "/api/catalog/feeds"),
  createFeed: (input: FeedInput) => api<Feed>("POST", "/api/catalog/feeds", input),
  updateFeed: (id: string, input: FeedInput) => api<Feed>("PATCH", `/api/catalog/feeds/${id}`, input),
  removeFeed: (id: string) => api<{ ok: true }>("DELETE", `/api/catalog/feeds/${id}`),
  syncFeed: (id: string) => api<{ ok: true; queued: boolean; note: string }>("POST", `/api/catalog/feeds/${id}/sync`),
  settings: () => api<CatalogSettings>("GET", "/api/catalog/settings"),
  updateSettings: (patch: CatalogSettingsPatch) => api<CatalogSettings>("PATCH", "/api/catalog/settings", patch),
};

export function useProducts(q: ProductQuery) {
  return useQuery({ queryKey: catalogKeys.products(q), queryFn: () => catalogApi.products(q), placeholderData: keepPreviousData });
}

export function useProduct(key: string) {
  return useQuery({ queryKey: catalogKeys.product(key), queryFn: () => catalogApi.product(key) });
}

export function useRelatedProducts(key: string) {
  return useQuery({ queryKey: catalogKeys.related(key), queryFn: () => catalogApi.related(key) });
}

export function useCatalogSummary() {
  return useQuery({ queryKey: catalogKeys.summary, queryFn: catalogApi.summary });
}

export function useCatalogFacets() {
  return useQuery({ queryKey: catalogKeys.facets, queryFn: catalogApi.facets, staleTime: 60_000 });
}

export function useDeleteProduct() {
  const qc = useQueryClient();
  return useMutation({ mutationFn: catalogApi.remove, onSuccess: () => qc.invalidateQueries({ queryKey: catalogKeys.all }) });
}

export function useImportProducts() {
  const qc = useQueryClient();
  return useMutation({ mutationFn: catalogApi.importCsv, onSuccess: () => qc.invalidateQueries({ queryKey: catalogKeys.all }) });
}

export function useFeeds() {
  // A sync runs in the background; check back while one is waiting.
  return useQuery({
    queryKey: catalogKeys.feeds,
    queryFn: catalogApi.feeds,
    select: (r) => r.data,
    refetchInterval: (q) => ((q.state.data?.data ?? []).some((f) => f.syncing) ? 3000 : false),
  });
}

export function useCreateFeed() {
  const qc = useQueryClient();
  return useMutation({ mutationFn: catalogApi.createFeed, onSuccess: () => qc.invalidateQueries({ queryKey: catalogKeys.all }) });
}

export function useUpdateFeed(id: string) {
  const qc = useQueryClient();
  return useMutation({ mutationFn: (input: FeedInput) => catalogApi.updateFeed(id, input), onSuccess: () => qc.invalidateQueries({ queryKey: catalogKeys.all }) });
}

export function useDeleteFeed() {
  const qc = useQueryClient();
  return useMutation({ mutationFn: catalogApi.removeFeed, onSuccess: () => qc.invalidateQueries({ queryKey: catalogKeys.all }) });
}

export function useSyncFeed() {
  const qc = useQueryClient();
  return useMutation({ mutationFn: catalogApi.syncFeed, onSuccess: () => qc.invalidateQueries({ queryKey: catalogKeys.feeds }) });
}

export function useCatalogSettings() {
  return useQuery({ queryKey: catalogKeys.settings, queryFn: catalogApi.settings });
}

export function useUpdateCatalogSettings() {
  const qc = useQueryClient();
  return useMutation({ mutationFn: catalogApi.updateSettings, onSuccess: (s) => qc.setQueryData(catalogKeys.settings, s) });
}
