import { useState } from "react";
import { X } from "lucide-react";
import { Checkbox, Field, IconButton, Input, Segmented, Select } from "../../components/ui";
import { useDebouncedValue } from "../../lib/hooks";
import { useCatalogFacets, useProduct, useProducts } from "./api";
import { ProductThumb } from "./parts";
import type { ProductsBlock, ProductsSource } from "./types";

export const sourceOptions: { value: ProductsSource; label: string; hint: string }[] = [
  { value: "event_items", label: "Items in the triggering event", hint: "The cart for checkout and cart flows, the viewed product for browse flows, the order's items after a purchase. A preview uses this person's latest cart." },
  { value: "recommended", label: "Recommended for this person", hint: "Bought or viewed together with their products, then popular in the categories they browse. Never what they already bought." },
  { value: "recently_viewed", label: "Recently viewed by this person", hint: "What they looked at, latest first." },
  { value: "best_sellers", label: "Best sellers", hint: "Bought by the most people lately." },
  { value: "newest", label: "Newest", hint: "Most recently published." },
  { value: "on_sale", label: "On sale", hint: "Biggest discount first." },
  { value: "collection", label: "A category, tag or brand", hint: "Newest first." },
  { value: "handpicked", label: "Products I pick", hint: "In the order you pick them." },
];

export const sourceLabel = (s: ProductsSource) => sourceOptions.find((o) => o.value === s)?.label ?? s;

export function newProductsBlock(): ProductsBlock {
  return { type: "products", source: "best_sellers", count: 4, columns: 2, show_price: true, show_compare_at: true, show_button: true, button_text: "Shop now", hide_out_of_stock: true };
}

function PickedChip({ id, onRemove }: { id: string; onRemove: () => void }) {
  const product = useProduct(id);
  const title = product.data?.title ?? (product.error ? `${id} (not found)` : id);
  return (
    <li className="inline-flex items-center gap-1 rounded-full bg-zinc-100 py-0.5 pl-2.5 pr-1 text-xs text-zinc-800">
      {title}
      <IconButton size="sm" label={`Remove ${title}`} onClick={onRemove}>
        <X className="size-3" />
      </IconButton>
    </li>
  );
}

function ProductPicker({ ids, onChange }: { ids: string[]; onChange: (ids: string[]) => void }) {
  const [search, setSearch] = useState("");
  const q = useDebouncedValue(search.trim(), 250);
  const results = useProducts({ q: q || undefined, limit: 8 });
  return (
    <div className="space-y-2">
      {ids.length ? (
        <ul className="flex flex-wrap gap-1.5">
          {ids.map((id) => (
            <PickedChip key={id} id={id} onRemove={() => onChange(ids.filter((x) => x !== id))} />
          ))}
        </ul>
      ) : null}
      {ids.length < 12 ? (
        <>
          <Input value={search} onChange={(e) => setSearch(e.target.value)} placeholder="Search products to add" aria-label="Search products" />
          <ul className="max-h-48 divide-y divide-zinc-100 overflow-y-auto rounded-md border border-zinc-200">
            {(results.data?.data ?? []).filter((p) => !ids.includes(p.external_id)).map((p) => (
              <li key={p.id}>
                <button type="button" onClick={() => onChange([...ids, p.external_id])} className="flex w-full items-center gap-2 px-2 py-1.5 text-left text-sm hover:bg-zinc-50">
                  <ProductThumb src={p.image_url} alt="" />
                  <span className="min-w-0 flex-1 truncate">{p.title}</span>
                  {!p.in_stock ? <span className="text-xs text-red-600">Sold out</span> : null}
                </button>
              </li>
            ))}
            {results.data && results.data.data.length === 0 ? <li className="px-3 py-2 text-xs text-zinc-500">No products found. Add products on the Products page.</li> : null}
          </ul>
        </>
      ) : null}
    </div>
  );
}

/** Editor for the `products` email block. */
export function ProductsBlockFields({ block, onChange }: { block: ProductsBlock; onChange: (b: ProductsBlock) => void }) {
  const facets = useCatalogFacets();
  const set = (patch: Partial<ProductsBlock>) => onChange({ ...block, ...patch });
  const hint = sourceOptions.find((o) => o.value === block.source)?.hint;
  const facetSelect = (key: "category" | "tag" | "brand", label: string, values: string[]) => (
    <Field label={label}>
      <Select value={block[key] ?? ""} onChange={(e) => set({ [key]: e.target.value || undefined })}>
        <option value="">Any</option>
        {values.map((v) => (
          <option key={v} value={v}>
            {v}
          </option>
        ))}
      </Select>
    </Field>
  );
  return (
    <div className="space-y-3">
      <Field label="Show" hint={hint}>
        <Select value={block.source} onChange={(e) => set({ source: e.target.value as ProductsSource })}>
          {sourceOptions.map((o) => (
            <option key={o.value} value={o.value}>
              {o.label}
            </option>
          ))}
        </Select>
      </Field>
      {block.source === "handpicked" ? <ProductPicker ids={block.product_ids ?? []} onChange={(product_ids) => set({ product_ids })} /> : null}
      {block.source === "collection" ? (
        <div className="grid grid-cols-1 gap-3 sm:grid-cols-3">
          {facetSelect("category", "Category", facets.data?.categories ?? [])}
          {facetSelect("tag", "Tag", facets.data?.tags ?? [])}
          {facetSelect("brand", "Brand", facets.data?.brands ?? [])}
        </div>
      ) : null}
      {block.source === "best_sellers" ? (
        <Field label="Over the last">
          <Select value={block.days ?? 30} onChange={(e) => set({ days: Number(e.target.value) })}>
            {[7, 30, 90, 365].map((d) => (
              <option key={d} value={d}>
                {d} days
              </option>
            ))}
          </Select>
        </Field>
      ) : null}
      <div className="grid grid-cols-1 gap-3 sm:grid-cols-2">
        <Field label="How many">
          <Select value={block.count ?? 4} onChange={(e) => set({ count: Number(e.target.value) })}>
            {Array.from({ length: 12 }, (_, i) => i + 1).map((n) => (
              <option key={n} value={n}>
                {n}
              </option>
            ))}
          </Select>
        </Field>
        <Field label="Across" hint="They stack on phones.">
          <Segmented
            size="sm"
            value={String(block.columns ?? 2) as "1" | "2" | "3" | "4"}
            onChange={(v) => set({ columns: Number(v) as 1 | 2 | 3 | 4 })}
            options={[
              { value: "1", label: "1" },
              { value: "2", label: "2" },
              { value: "3", label: "3" },
              { value: "4", label: "4" },
            ]}
          />
        </Field>
        <Field label="Button text">
          <Input value={block.button_text ?? ""} maxLength={40} onChange={(e) => set({ button_text: e.target.value || undefined })} placeholder="Shop now" />
        </Field>
        <Field label="If there's nothing to show" hint="Otherwise the block is left out.">
          <Select value={block.fallback ?? ""} onChange={(e) => set({ fallback: (e.target.value || undefined) as ProductsSource | undefined })}>
            <option value="">Leave it out</option>
            {sourceOptions
              .filter((o) => o.value !== block.source && o.value !== "handpicked" && o.value !== "collection")
              .map((o) => (
                <option key={o.value} value={o.value}>
                  Show {o.label.toLowerCase()}
                </option>
              ))}
          </Select>
        </Field>
      </div>
      <div className="grid grid-cols-1 gap-2 sm:grid-cols-2">
        <Checkbox label="Show price" checked={block.show_price ?? true} onChange={(e) => set({ show_price: e.target.checked })} />
        <Checkbox label="Show the original price when on sale" checked={block.show_compare_at ?? true} onChange={(e) => set({ show_compare_at: e.target.checked })} />
        <Checkbox label="Show a button" checked={block.show_button ?? true} onChange={(e) => set({ show_button: e.target.checked })} />
        <Checkbox label="Hide sold-out products" checked={block.hide_out_of_stock ?? true} onChange={(e) => set({ hide_out_of_stock: e.target.checked })} />
      </div>
    </div>
  );
}
