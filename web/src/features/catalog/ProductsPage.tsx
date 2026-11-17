import { useState, type ChangeEvent } from "react";
import { useSearchParams } from "react-router";
import { ChevronLeft, ChevronRight, Package, Rss, Search, Upload } from "lucide-react";
import { useSession } from "../../api";
import { Button, ButtonLink, EmptyRow, EmptyState, Field, IconButton, Input, Modal, Notice, Page, PageHeader, RowLink, Select, SkeletonRows, Table, Td, Textarea, Th } from "../../components/ui";
import { formatDate, formatNumber } from "../../lib/format";
import { useDebouncedValue } from "../../lib/hooks";
import { toast } from "../../lib/toast";
import { useCatalogFacets, useCatalogSummary, useImportProducts, useProducts } from "./api";
import { CatalogTabs, PriceText, ProductThumb, SourceBadge, StockBadge } from "./parts";
import type { ProductQuery, UpsertSummary } from "./types";

const limit = 50;

export function ProductsPage() {
  const { account } = useSession();
  const [params, setParams] = useSearchParams();
  const [search, setSearch] = useState("");
  const q = useDebouncedValue(search.trim(), 250);
  const category = params.get("category") ?? "";
  const stock = params.get("stock") ?? "";
  const filters: ProductQuery = { q: q || undefined, category: category || undefined, in_stock: stock === "" ? undefined : stock === "in" };
  const filterKey = JSON.stringify(filters);
  // Paging belongs to one set of filters; changing them starts at the first page.
  const [paging, setPaging] = useState({ key: "", offset: 0 });
  const offset = paging.key === filterKey ? paging.offset : 0;
  const products = useProducts({ ...filters, limit, offset });
  const summary = useCatalogSummary();
  const facets = useCatalogFacets();
  const importing = params.get("import") === "1";
  const filtered = Boolean(q || category || stock);

  const setParam = (k: string, v: string) => {
    const next = new URLSearchParams(params);
    if (v) next.set(k, v);
    else next.delete(k);
    setParams(next, { replace: true });
  };

  const s = summary.data;
  const total = products.data?.total ?? 0;
  return (
    <Page>
      <PageHeader
        title="Products"
        description={s ? `${formatNumber(s.products)} products · ${formatNumber(s.in_stock)} in stock` : "Your catalog, for product blocks, recommendations and alerts."}
        actions={
          <>
            <Button icon={<Upload className="size-3.5" />} onClick={() => setParam("import", "1")}>
              Upload CSV
            </Button>
            <ButtonLink to="/products/feeds?add=1" variant="primary" icon={<Rss className="size-3.5" />}>
              Add a feed
            </ButtonLink>
          </>
        }
      />
      <CatalogTabs />
      {s && s.learned_from_events > 0 && s.learned_from_events === s.products ? (
        <div className="mb-4">
          <Notice tone="blue" title="These products came from your website">
            DumbMail adds the products people view and buy on your site. Add a feed or upload a CSV for full details like stock, sale prices and variants.
          </Notice>
        </div>
      ) : null}
      <div className="mb-3 flex flex-wrap items-center gap-2">
        <div className="relative w-full max-w-sm">
          <Search className="pointer-events-none absolute left-2.5 top-2.5 size-3.5 text-zinc-400" />
          <Input value={search} onChange={(e) => setSearch(e.target.value)} placeholder="Search by name, ID, SKU or brand" aria-label="Search products" className="pl-8" />
        </div>
        <Select value={category} onChange={(e) => setParam("category", e.target.value)} className="w-auto" aria-label="Category">
          <option value="">All categories</option>
          {(facets.data?.categories ?? []).map((c) => (
            <option key={c} value={c}>
              {c}
            </option>
          ))}
        </Select>
        <Select value={stock} onChange={(e) => setParam("stock", e.target.value)} className="w-auto" aria-label="Stock">
          <option value="">Any stock</option>
          <option value="in">In stock</option>
          <option value="out">Sold out</option>
        </Select>
      </div>
      <Table>
        <thead>
          <tr>
            <Th>Product</Th>
            <Th>Price</Th>
            <Th>Stock</Th>
            <Th className="hidden md:table-cell">Category</Th>
            <Th className="hidden sm:table-cell">Source</Th>
            <Th className="hidden lg:table-cell">Updated</Th>
          </tr>
        </thead>
        <tbody>
          {products.isPending ? (
            <SkeletonRows cols={6} rows={8} />
          ) : products.error ? (
            <EmptyRow cols={6}>
              <EmptyState title="Couldn't load products" description={products.error.message} />
            </EmptyRow>
          ) : (products.data?.data ?? []).length === 0 ? (
            <EmptyRow cols={6}>
              {filtered ? (
                <EmptyState icon={<Search className="size-5" />} title="No matches" description="No products match these filters." />
              ) : (
                <EmptyState
                  icon={<Package className="size-5" />}
                  title="No products yet"
                  description="Add a product feed, upload a CSV, or send products through the API. With the DumbMail snippet on your site, products people view and buy are added on their own."
                  action={
                    <ButtonLink to="/products/feeds?add=1" variant="primary">
                      Add a feed
                    </ButtonLink>
                  }
                />
              )}
            </EmptyRow>
          ) : (
            (products.data?.data ?? []).map((p) => (
              <RowLink key={p.id} to={`/products/${p.id}`}>
                <Td>
                  <div className="flex min-w-0 items-center gap-3">
                    <ProductThumb src={p.image_url} alt="" />
                    <div className="min-w-0">
                      <div className="truncate font-medium text-zinc-900">{p.title}</div>
                      <div className="truncate text-xs text-zinc-500">
                        {p.external_id}
                        {!p.published ? " · Unpublished" : ""}
                      </div>
                    </div>
                  </div>
                </Td>
                <Td>
                  <PriceText price={p.price} compareAt={p.compare_at_price} currency={p.currency} />
                </Td>
                <Td>
                  <StockBadge item={p} />
                </Td>
                <Td className="hidden max-w-48 truncate text-zinc-600 md:table-cell">{p.categories.join(", ") || "—"}</Td>
                <Td className="hidden sm:table-cell">
                  <SourceBadge source={p.source} />
                </Td>
                <Td className="hidden whitespace-nowrap text-zinc-600 lg:table-cell">{formatDate(p.updated_at, account.timezone)}</Td>
              </RowLink>
            ))
          )}
        </tbody>
      </Table>
      {total > limit ? (
        <div className="mt-3 flex items-center justify-end gap-2 text-xs text-zinc-500">
          <span className="tabular-nums">
            {formatNumber(offset + 1)}–{formatNumber(Math.min(offset + limit, total))} of {formatNumber(total)}
          </span>
          <IconButton label="Previous page" disabled={offset === 0} onClick={() => setPaging({ key: filterKey, offset: Math.max(0, offset - limit) })}>
            <ChevronLeft className="size-4" />
          </IconButton>
          <IconButton label="Next page" disabled={offset + limit >= total} onClick={() => setPaging({ key: filterKey, offset: offset + limit })}>
            <ChevronRight className="size-4" />
          </IconButton>
        </div>
      ) : null}
      {importing ? <ImportProductsDialog onClose={() => setParam("import", "")} /> : null}
    </Page>
  );
}

const example = "id,title,price,url,image,inventory,categories,tags\nTEE-1,Organic tee,24,https://shop.example.com/tee,https://cdn.example.com/tee.jpg,12,Shirts,organic";

function ImportProductsDialog({ onClose }: { onClose: () => void }) {
  const [csv, setCsv] = useState("");
  const [result, setResult] = useState<UpsertSummary | null>(null);
  const importer = useImportProducts();
  const onFile = (e: ChangeEvent<HTMLInputElement>) => {
    const f = e.target.files?.[0];
    if (!f) return;
    if (f.size > 20 * 1024 * 1024) {
      toast.error("That file is larger than 20 MB. Split it into smaller files.");
      return;
    }
    void f.text().then(setCsv);
  };
  return (
    <Modal
      title="Upload products"
      description="A CSV with a header row. Rows that share an id and have a variant_id become variants. Other columns become custom fields."
      onClose={onClose}
      size="lg"
      footer={
        result ? (
          <Button variant="primary" onClick={onClose}>
            Done
          </Button>
        ) : (
          <>
            <Button variant="ghost" onClick={onClose}>
              Cancel
            </Button>
            <Button variant="primary" loading={importer.isPending} disabled={!csv.trim()} onClick={() => importer.mutate(csv, { onSuccess: setResult })}>
              Import
            </Button>
          </>
        )
      }
    >
      {result ? (
        <div className="space-y-3 text-sm">
          <p className="text-zinc-800">
            {formatNumber(result.created)} added, {formatNumber(result.updated)} updated, {formatNumber(result.unchanged)} unchanged
            {result.failed ? `, ${formatNumber(result.failed)} skipped` : ""}.
          </p>
          {result.errors.length ? (
            <ul className="max-h-48 list-disc space-y-1 overflow-y-auto pl-5 text-xs text-red-700">
              {result.errors.map((e) => (
                <li key={e}>{e}</li>
              ))}
            </ul>
          ) : null}
        </div>
      ) : (
        <div className="space-y-3">
          <Field label="CSV file">
            <input type="file" accept=".csv,text/csv" onChange={onFile} className="block w-full text-sm text-zinc-700 file:mr-3 file:rounded-md file:border-0 file:bg-zinc-100 file:px-3 file:py-1.5 file:text-sm file:font-medium" />
          </Field>
          <Field label="Or paste it" hint="Columns: id, title, description, url, image, price, compare_at_price, currency, inventory, available, categories, tags, brand, vendor, published, variant_id, variant_title, sku.">
            <Textarea rows={8} value={csv} onChange={(e) => setCsv(e.target.value)} placeholder={example} className="font-mono text-xs" />
          </Field>
        </div>
      )}
    </Modal>
  );
}
