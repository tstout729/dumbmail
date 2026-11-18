import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { api } from "../../api/client";
import type { ShopifyConnectResponse, ShopifyImport, ShopifyStatus } from "./types";

export const shopifyKeys = { status: ["shopify", "status"] as const };

export const shopifyApi = {
  status: () => api<ShopifyStatus>("GET", "/api/shopify"),
  connect: (shop: string) => api<ShopifyConnectResponse>("POST", "/api/shopify/connect", { shop }),
  disconnect: () => api<{ ok: true; cleaned_up: boolean; not_removed: string[]; note: string }>("POST", "/api/shopify/disconnect", { confirm: true }),
  startImport: () => api<ShopifyImport>("POST", "/api/shopify/import", { confirm: true }),
  importAction: (action: "pause" | "resume" | "cancel") => api<ShopifyImport>("POST", `/api/shopify/import/${action}`),
  resyncProducts: () => api<{ products_sync: ShopifyImport; note: string }>("POST", "/api/shopify/products/resync"),
  dataRequest: (id: string) => api<unknown>("GET", `/api/shopify/data-requests/${encodeURIComponent(id)}`),
};

function busy(s: ShopifyStatus | undefined): boolean {
  return s?.import?.status === "running" || s?.products_sync?.status === "running";
}

/** Refreshes every few seconds while an import or product sync runs. */
export function useShopifyStatus() {
  return useQuery({
    queryKey: shopifyKeys.status,
    queryFn: shopifyApi.status,
    refetchInterval: (q) => (busy(q.state.data) ? 4000 : 30000),
  });
}

function useShopifyMutation<A, R>(fn: (a: A) => Promise<R>) {
  const qc = useQueryClient();
  return useMutation({ mutationFn: fn, onSettled: () => qc.invalidateQueries({ queryKey: shopifyKeys.status }) });
}

export const useConnectShopify = () => useMutation({ mutationFn: shopifyApi.connect });
export const useDisconnectShopify = () => useShopifyMutation(shopifyApi.disconnect);
export const useStartShopifyImport = () => useShopifyMutation(shopifyApi.startImport);
export const useShopifyImportAction = () => useShopifyMutation(shopifyApi.importAction);
export const useResyncShopifyProducts = () => useShopifyMutation(shopifyApi.resyncProducts);
