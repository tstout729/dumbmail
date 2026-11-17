import { useLocation, useNavigate } from "react-router";
import { Package } from "lucide-react";
import { Badge, Tabs, cx } from "../../components/ui";
import type { Product, ProductSource, Variant } from "./types";

/** A price in the product's currency ("$24.00", "€9.50"). */
export function money(value: number | null | undefined, currency?: string | null): string {
  if (value === null || value === undefined) return "—";
  try {
    return new Intl.NumberFormat("en-US", { style: "currency", currency: currency || "USD" }).format(value);
  } catch {
    return `${value.toFixed(2)} ${currency ?? ""}`.trim();
  }
}

export function PriceText({ price, compareAt, currency }: { price: number | null; compareAt: number | null; currency: string | null }) {
  const onSale = price !== null && compareAt !== null && compareAt > price;
  return (
    <span className="whitespace-nowrap tabular-nums">
      {money(price, currency)}
      {onSale ? <span className="ml-1.5 text-xs text-zinc-400 line-through">{money(compareAt, currency)}</span> : null}
    </span>
  );
}

export function StockBadge({ item }: { item: Pick<Product | Variant, "in_stock" | "inventory"> }) {
  if (!item.in_stock) return <Badge tone="red" dot>Sold out</Badge>;
  return (
    <Badge tone="green" dot>
      {item.inventory !== null ? `${item.inventory} in stock` : "In stock"}
    </Badge>
  );
}

const sourceLabels: Record<ProductSource, string> = { api: "API", csv: "CSV", feed: "Feed", events: "From your site" };

export function SourceBadge({ source }: { source: ProductSource }) {
  return <Badge tone={source === "events" ? "violet" : "gray"}>{sourceLabels[source]}</Badge>;
}

export function ProductThumb({ src, alt, size = "md" }: { src: string | null; alt: string; size?: "md" | "lg" }) {
  const box = size === "lg" ? "size-40 sm:size-48" : "size-10";
  if (!src) {
    return (
      <div className={cx("flex shrink-0 items-center justify-center rounded-md bg-zinc-100 text-zinc-400", box)}>
        <Package className={size === "lg" ? "size-8" : "size-4"} />
      </div>
    );
  }
  return <img src={src} alt={alt} loading="lazy" className={cx("shrink-0 rounded-md border border-zinc-100 bg-white object-cover", box)} />;
}

const tabs = [
  { value: "/products", label: "Products" },
  { value: "/products/feeds", label: "Feeds" },
  { value: "/products/alerts", label: "Alerts" },
] as const;

type TabPath = (typeof tabs)[number]["value"];

/** Products, Feeds and Alerts share one header. */
export function CatalogTabs() {
  const { pathname } = useLocation();
  const navigate = useNavigate();
  const current: TabPath = pathname.startsWith("/products/feeds") ? "/products/feeds" : pathname.startsWith("/products/alerts") ? "/products/alerts" : "/products";
  return <Tabs<TabPath> value={current} onChange={(to) => navigate(to)} tabs={tabs.map((t) => ({ value: t.value, label: t.label }))} />;
}
