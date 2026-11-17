// Catalog types. They match the JSON from src/features/catalog (see docs/CATALOG.md).

export type ProductSource = "api" | "csv" | "feed" | "events";

export type Variant = {
  id: string;
  external_id: string;
  title: string | null;
  options: Record<string, unknown>;
  sku: string | null;
  price: number | null;
  compare_at_price: number | null;
  available: boolean;
  inventory: number | null;
  in_stock: boolean;
  image_url: string | null;
};

export type Product = {
  id: string;
  external_id: string;
  title: string;
  description: string | null;
  url: string | null;
  image_url: string | null;
  images: string[];
  price: number | null;
  compare_at_price: number | null;
  currency: string | null;
  available: boolean;
  inventory: number | null;
  in_stock: boolean;
  categories: string[];
  tags: string[];
  brand: string | null;
  vendor: string | null;
  custom: Record<string, unknown>;
  published: boolean;
  published_at: string;
  source: ProductSource;
  feed_id: string | null;
  created_at: string;
  updated_at: string;
};

export type PriceHistoryEntry = {
  variant_id: string | null;
  price: number | null;
  compare_at_price: number | null;
  in_stock: boolean;
  inventory: number | null;
  recorded_at: string;
};

export type ProductDetail = Product & {
  variants: Variant[];
  history: PriceHistoryEntry[];
  waiting: { profile_id: string; email: string | null; variant_id: string | null; since: string }[];
  waiting_total: number;
  activity_30d: Partial<Record<"viewed" | "carted" | "checkout" | "ordered", number>>;
};

export type ProductQuery = {
  q?: string;
  category?: string;
  tag?: string;
  brand?: string;
  in_stock?: boolean;
  on_sale?: boolean;
  limit?: number;
  offset?: number;
};

export type CatalogSummary = {
  products: number;
  in_stock: number;
  published: number;
  learned_from_events: number;
  waiting_for_restock: number;
};

export type CatalogFacets = { categories: string[]; tags: string[]; brands: string[] };

export type UpsertSummary = { created: number; updated: number; unchanged: number; failed: number; errors: string[] };

export type FeedFormat = "json" | "google_xml";

export type Feed = {
  id: string;
  name: string;
  url: string;
  format: FeedFormat;
  field_map: Record<string, string>;
  interval_hours: number;
  enabled: boolean;
  unpublish_missing: boolean;
  next_sync_at: string;
  last_synced_at: string | null;
  last_status: "ok" | "not_modified" | "error" | null;
  last_error: string | null;
  last_counts: Partial<Record<"fetched" | "created" | "updated" | "unchanged" | "failed" | "unpublished", number>>;
  products?: number;
  syncing?: boolean;
  created_at: string;
  updated_at: string;
};

export type FeedInput = Partial<Pick<Feed, "name" | "url" | "format" | "field_map" | "interval_hours" | "enabled" | "unpublish_missing">>;

export type WidgetSettings = { button_text?: string; heading?: string; success_message?: string; color?: string; text_color?: string };

export type CatalogSettings = {
  learn_from_events: boolean;
  back_in_stock_enabled: boolean;
  back_in_stock_limit: number | null;
  price_drop_enabled: boolean;
  price_drop_percent: number | null;
  price_drop_amount: number | null;
  price_drop_lookback_days: number;
  price_drop_cooldown_days: number;
  widget: WidgetSettings;
  widget_snippet: string;
};

export type CatalogSettingsPatch = Partial<Omit<CatalogSettings, "widget_snippet">>;

/** A product as emails and templates see it. */
export type ProductCard = {
  id: string | null;
  title: string | null;
  url: string | null;
  image_url: string | null;
  price: number | null;
  price_text: string | null;
  compare_at_text: string | null;
  on_sale: boolean;
  in_stock: boolean;
};

export type ProductRecommendations = { product: ProductCard; bought_together: ProductCard[]; viewed_together: ProductCard[]; how: string };

export type ProductsSource = "handpicked" | "collection" | "best_sellers" | "newest" | "on_sale" | "event_items" | "recently_viewed" | "recommended";

/** The email builder's dynamic product grid, filled per recipient when it sends. */
export type ProductsBlock = {
  type: "products";
  source: ProductsSource;
  product_ids?: string[];
  category?: string;
  tag?: string;
  brand?: string;
  days?: number;
  count?: number;
  columns?: 1 | 2 | 3 | 4;
  show_price?: boolean;
  show_compare_at?: boolean;
  show_button?: boolean;
  button_text?: string;
  fallback?: ProductsSource;
  hide_out_of_stock?: boolean;
};

export type ProductAction = "viewed" | "carted" | "checkout" | "ordered";

/** Segment rule: did something with one product, or any product in a category, with a tag or of a brand. */
export type ProductCondition = {
  type: "product";
  action: ProductAction;
  product_id?: string;
  category?: string;
  tag?: string;
  brand?: string;
  op: "at_least" | "at_most" | "exactly" | "zero";
  count?: number;
  within_days?: number;
  since?: string;
  until?: string;
};
