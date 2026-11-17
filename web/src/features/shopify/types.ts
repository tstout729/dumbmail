export type ShopifyHealthState = "server" | "quiet" | "waiting" | "browser_only" | "none";

export type ShopifyConnection = {
  shop: string;
  status: "connected" | "uninstalled" | "disconnected";
  name: string | null;
  currency: string | null;
  store_url: string | null;
  scopes: string[];
  token_hint: string | null;
  connected_at: string;
  disconnected_at: string | null;
  last_webhook_at: string | null;
  last_webhook_topic: string | null;
  last_order_at: string | null;
  last_error: string | null;
  last_error_at: string | null;
  web_pixel_installed: boolean;
  storefront_error: string | null;
};

export type ShopifyImportStatus = "running" | "paused" | "cancelled" | "done" | "failed";
export type ShopifyImportPhase = "customers" | "products" | "orders" | "done";

export type ShopifyImport = {
  id: string;
  kind: "history" | "products";
  status: ShopifyImportStatus;
  phase: ShopifyImportPhase;
  phases: ShopifyImportPhase[];
  orders_since: string;
  counts: { customers?: number; products?: number; orders?: number; shopify_objects?: number };
  shopify_status: string | null;
  error: string | null;
  started_at: string;
  finished_at: string | null;
  updated_at: string;
};

export type ShopifyDataRequest = { id: string; customer_email: string | null; created_at: string; downloaded_at: string | null };

export type ShopifyStatus = {
  available: boolean;
  connected: boolean;
  connection: ShopifyConnection | null;
  health: { state: ShopifyHealthState; message: string };
  syncing: string[];
  counts: { linked_customers: number; products: number; webhooks_waiting: number };
  import: ShopifyImport | null;
  products_sync: ShopifyImport | null;
  storefront: { embed_link: string | null; pixel_installed: boolean; error: string | null };
  data_requests: ShopifyDataRequest[];
  webhook_url: string;
  redirect_url: string;
  app_url: string;
};

export type ShopifyConnectResponse = { url: string; shop: string; expires_in_minutes: number; note: string };
