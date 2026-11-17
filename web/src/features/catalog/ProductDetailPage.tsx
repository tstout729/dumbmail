import { useState } from "react";
import { Link, useNavigate, useParams } from "react-router";
import { ExternalLink, Trash2 } from "lucide-react";
import { useSession } from "../../api";
import {
  Badge,
  Button,
  Card,
  ConfirmDialog,
  EmptyState,
  Page,
  PageHeader,
  Skeleton,
  Stat,
  Table,
  Td,
  Th,
} from "../../components/ui";
import { formatDateTime, formatNumber } from "../../lib/format";
import { toast } from "../../lib/toast";
import { useDeleteProduct, useProduct, useRelatedProducts } from "./api";
import {
  PriceText,
  ProductThumb,
  SourceBadge,
  StockBadge,
  money,
} from "./parts";
import type { ProductCard, ProductDetail } from "./types";

export function ProductDetailPage() {
  const { id = "" } = useParams();
  const product = useProduct(id);
  if (product.isPending) {
    return (
      <Page>
        <Skeleton className="mb-5 h-8 w-64" />
        <Skeleton className="h-64 w-full" />
      </Page>
    );
  }
  if (product.error || !product.data) {
    return (
      <Page>
        <PageHeader
          title="Product"
          breadcrumbs={[{ label: "Products", to: "/products" }]}
        />
        <Card>
          <EmptyState
            title="Product not found"
            description={product.error?.message ?? "It may have been deleted."}
          />
        </Card>
      </Page>
    );
  }
  return <ProductView p={product.data} />;
}

function ProductView({ p }: { p: ProductDetail }) {
  const { account } = useSession();
  const navigate = useNavigate();
  const remove = useDeleteProduct();
  const [confirming, setConfirming] = useState(false);
  const variantName = (vid: string | null) =>
    vid
      ? (p.variants.find((v) => v.id === vid)?.title ?? "Variant")
      : "Product";
  const a = p.activity_30d;
  return (
    <Page>
      <PageHeader
        title={p.title}
        breadcrumbs={[{ label: "Products", to: "/products" }]}
        description={
          <span className="flex flex-wrap items-center gap-2">
            <span>{p.external_id}</span>
            <SourceBadge source={p.source} />
            {!p.published ? <Badge tone="amber">Unpublished</Badge> : null}
          </span>
        }
        actions={
          <>
            {p.url ? (
              <a
                href={p.url}
                target="_blank"
                rel="noreferrer"
                className="inline-flex items-center gap-1 text-sm font-medium text-zinc-600 hover:text-zinc-900"
              >
                View on site <ExternalLink className="size-3.5" />
              </a>
            ) : null}
            <Button
              variant="ghost"
              icon={<Trash2 className="size-3.5" />}
              onClick={() => setConfirming(true)}
            >
              Delete
            </Button>
          </>
        }
      />
      <div className="grid grid-cols-1 gap-4 lg:grid-cols-3">
        <Card className="lg:col-span-2">
          <div className="flex flex-col gap-4 sm:flex-row">
            <ProductThumb src={p.image_url} alt={p.title} size="lg" />
            <dl className="grid min-w-0 flex-1 grid-cols-[auto_minmax(0,1fr)] gap-x-4 gap-y-2 text-sm">
              <dt className="text-zinc-500">Price</dt>
              <dd className="text-zinc-900">
                <PriceText
                  price={p.price}
                  compareAt={p.compare_at_price}
                  currency={p.currency}
                />
              </dd>
              <dt className="text-zinc-500">Stock</dt>
              <dd>
                <StockBadge item={p} />
              </dd>
              <dt className="text-zinc-500">Categories</dt>
              <dd className="text-zinc-900">
                {p.categories.join(", ") || "—"}
              </dd>
              <dt className="text-zinc-500">Tags</dt>
              <dd className="flex flex-wrap gap-1">
                {p.tags.length
                  ? p.tags.map((t) => <Badge key={t}>{t}</Badge>)
                  : "—"}
              </dd>
              <dt className="text-zinc-500">Brand</dt>
              <dd className="text-zinc-900">{p.brand ?? p.vendor ?? "—"}</dd>
              <dt className="text-zinc-500">Updated</dt>
              <dd className="text-zinc-900">
                {formatDateTime(p.updated_at, account.timezone)}
              </dd>
              {Object.keys(p.custom).length ? (
                <>
                  <dt className="text-zinc-500">Custom</dt>
                  <dd className="break-words font-mono text-xs text-zinc-700">
                    {JSON.stringify(p.custom)}
                  </dd>
                </>
              ) : null}
            </dl>
          </div>
        </Card>
        <div className="grid grid-cols-2 gap-3 self-start">
          <Stat label="Views (30 days)" value={formatNumber(a.viewed ?? 0)} />
          <Stat label="Added to cart" value={formatNumber(a.carted ?? 0)} />
          <Stat label="Orders" value={formatNumber(a.ordered ?? 0)} />
          <Stat
            label="Waiting for restock"
            value={formatNumber(p.waiting_total)}
          />
        </div>
      </div>

      {p.variants.length ? (
        <Card
          title={`Variants (${p.variants.length})`}
          padded={false}
          className="mt-4"
        >
          <div className="overflow-x-auto">
            <Table className="rounded-none border-0 shadow-none">
              <thead>
                <tr>
                  <Th>Variant</Th>
                  <Th>SKU</Th>
                  <Th>Price</Th>
                  <Th>Stock</Th>
                </tr>
              </thead>
              <tbody>
                {p.variants.map((v) => (
                  <tr key={v.id}>
                    <Td>
                      <div className="font-medium text-zinc-900">
                        {v.title ?? v.external_id}
                      </div>
                      <div className="text-xs text-zinc-500">
                        {v.external_id}
                      </div>
                    </Td>
                    <Td className="text-zinc-600">{v.sku ?? "—"}</Td>
                    <Td>
                      <PriceText
                        price={v.price}
                        compareAt={v.compare_at_price}
                        currency={p.currency}
                      />
                    </Td>
                    <Td>
                      <StockBadge item={v} />
                    </Td>
                  </tr>
                ))}
              </tbody>
            </Table>
          </div>
        </Card>
      ) : null}

      <div className="mt-4 grid grid-cols-1 gap-4 xl:grid-cols-[minmax(0,3fr)_minmax(0,2fr)]">
        <Card title="Price and stock history" padded={false}>
          {p.history.length ? (
            <div className="max-h-96 overflow-auto">
              <Table className="rounded-none border-0 shadow-none">
                <thead>
                  <tr>
                    <Th>When</Th>
                    <Th>What</Th>
                    <Th>Price</Th>
                    <Th>Stock</Th>
                  </tr>
                </thead>
                <tbody>
                  {p.history.map((h, i) => (
                    <tr key={`${h.recorded_at}-${i}`}>
                      <Td className="whitespace-nowrap text-zinc-600">
                        {formatDateTime(h.recorded_at, account.timezone)}
                      </Td>
                      <Td className="text-zinc-700">
                        {variantName(h.variant_id)}
                      </Td>
                      <Td>
                        {h.price !== null ? money(h.price, p.currency) : "—"}
                      </Td>
                      <Td>
                        {h.in_stock ? (
                          <span className="text-accent-800">
                            In stock
                            {h.inventory !== null ? ` (${h.inventory})` : ""}
                          </span>
                        ) : (
                          <span className="text-red-700">Sold out</span>
                        )}
                      </Td>
                    </tr>
                  ))}
                </tbody>
              </Table>
            </div>
          ) : (
            <EmptyState
              title="No changes yet"
              description="Price and stock changes from your feed, CSV or the API show here."
            />
          )}
        </Card>
        <Card
          title={`Waiting for a restock (${formatNumber(p.waiting_total)})`}
          padded={false}
        >
          {p.waiting.length ? (
            <ul className="max-h-96 divide-y divide-zinc-100 overflow-y-auto">
              {p.waiting.map((w) => (
                <li
                  key={`${w.profile_id}-${w.variant_id ?? ""}`}
                  className="flex items-center justify-between gap-3 px-4 py-2.5 text-sm"
                >
                  <Link
                    to={`/audience/profiles/${w.profile_id}`}
                    className="min-w-0 truncate font-medium text-zinc-900 hover:underline"
                  >
                    {w.email ?? "Profile"}
                  </Link>
                  <span className="shrink-0 text-xs text-zinc-500">
                    {w.variant_id ? variantName(w.variant_id) : "Any option"} ·{" "}
                    {formatDateTime(w.since, account.timezone)}
                  </span>
                </li>
              ))}
            </ul>
          ) : (
            <EmptyState
              title="Nobody is waiting"
              description="People who ask to be told when it's back appear here."
            />
          )}
        </Card>
      </div>

      <Related productId={p.id} />

      {confirming ? (
        <ConfirmDialog
          title="Delete this product?"
          body={`This removes ${p.title} with its variants, history and ${p.waiting_total} waiting restock request(s). If a feed still lists it, the next sync adds it back.`}
          confirmLabel="Delete"
          danger
          loading={remove.isPending}
          onConfirm={() =>
            remove.mutate(p.id, {
              onSuccess: () => {
                toast.success("Product deleted");
                navigate("/products");
              },
            })
          }
          onClose={() => setConfirming(false)}
        />
      ) : null}
    </Page>
  );
}

function CardRow({ items }: { items: ProductCard[] }) {
  if (!items.length)
    return (
      <p className="text-sm text-zinc-500">Not enough orders or views yet.</p>
    );
  return (
    <ul className="grid grid-cols-2 gap-3 sm:grid-cols-3">
      {items.map((c) => (
        <li key={c.id ?? c.title} className="min-w-0">
          <ProductThumb src={c.image_url} alt="" />
          <div className="mt-1 truncate text-sm font-medium text-zinc-900">
            {c.title}
          </div>
          <div className="text-xs text-zinc-500">{c.price_text ?? ""}</div>
        </li>
      ))}
    </ul>
  );
}

function Related({ productId }: { productId: string }) {
  const related = useRelatedProducts(productId);
  if (!related.data) return null;
  return (
    <Card title="Recommended with this product" className="mt-4">
      <p className="mb-3 text-xs text-zinc-500">{related.data.how}</p>
      <div className="grid grid-cols-1 gap-6 md:grid-cols-2">
        <div>
          <h3 className="mb-2 text-xs font-semibold uppercase tracking-wide text-zinc-500">
            Bought together
          </h3>
          <CardRow items={related.data.bought_together} />
        </div>
        <div>
          <h3 className="mb-2 text-xs font-semibold uppercase tracking-wide text-zinc-500">
            Viewed together
          </h3>
          <CardRow items={related.data.viewed_together} />
        </div>
      </div>
    </Card>
  );
}
