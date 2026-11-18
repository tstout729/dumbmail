import { useState } from "react";
import { Package, RefreshCw, Search } from "lucide-react";
import { Button, EmptyRow, EmptyState, Input, SkeletonRows, Table, Td, Th } from "../../components/ui";
import { formatNumber } from "../../lib/format";
import { useDebouncedValue } from "../../lib/hooks";
import { toast } from "../../lib/toast";
import { useCan } from "../team/api";
import { useRebuild, useReviewProducts } from "./api";
import { Stars } from "./parts";

const kindLabel = { product: "Product", listing: "Listing", service: "Service" } as const;

export function ProductsTab() {
  const [search, setSearch] = useState("");
  const q = useDebouncedValue(search.trim(), 250);
  const products = useReviewProducts(q);
  const rebuild = useRebuild();
  const canManage = useCan("reviews.manage");
  const rows = products.data?.data ?? [];
  return (
    <div className="space-y-3">
      <div className="flex flex-wrap items-center justify-between gap-2">
        <div className="relative w-full max-w-xs">
          <Search className="pointer-events-none absolute left-2.5 top-2.5 size-3.5 text-zinc-400" />
          <Input value={search} onChange={(e) => setSearch(e.target.value)} placeholder="Search products" aria-label="Search products" className="w-full pl-8" />
        </div>
        {canManage ? (
          <Button icon={<RefreshCw className="size-3.5" />} loading={rebuild.isPending} onClick={() => rebuild.mutate(undefined, { onSuccess: (r) => toast.success(r.note), onError: (e) => toast.error(e.message) })}>
            Recount stars
          </Button>
        ) : null}
      </div>
      <Table>
        <thead>
          <tr>
            <Th>Product</Th>
            <Th>Rating</Th>
            <Th className="hidden sm:table-cell">Published</Th>
            <Th className="hidden sm:table-cell">Waiting</Th>
            <Th className="hidden md:table-cell">Id</Th>
          </tr>
        </thead>
        <tbody>
          {products.isPending ? (
            <SkeletonRows cols={5} rows={4} />
          ) : products.error ? (
            <EmptyRow cols={5}>
              <EmptyState title="Couldn't load products" description={products.error.message} />
            </EmptyRow>
          ) : rows.length === 0 ? (
            <EmptyRow cols={5}>
              <EmptyState icon={<Package className="size-5" />} title="No reviewed products yet" description="Products show up here once someone is asked to review them." />
            </EmptyRow>
          ) : (
            rows.map((p) => (
              <tr key={p.id}>
                <Td>
                  <div className="flex min-w-0 items-center gap-2.5">
                    {p.image_url ? <img src={p.image_url} alt="" className="size-9 shrink-0 rounded object-cover ring-1 ring-zinc-200" loading="lazy" /> : <div className="size-9 shrink-0 rounded bg-zinc-100" />}
                    <div className="min-w-0">
                      <div className="truncate font-medium text-zinc-900">{p.name}</div>
                      <div className="text-xs text-zinc-500">{kindLabel[p.kind]}</div>
                    </div>
                  </div>
                </Td>
                <Td>
                  <div className="flex items-center gap-1.5 whitespace-nowrap">
                    <Stars value={p.summary.average} />
                    <span className="tabular-nums text-zinc-700">{p.summary.average != null ? p.summary.average.toFixed(1) : "–"}</span>
                  </div>
                </Td>
                <Td className="hidden tabular-nums sm:table-cell">{formatNumber(p.summary.count)}</Td>
                <Td className="hidden tabular-nums sm:table-cell">{p.pending ? formatNumber(p.pending) : "–"}</Td>
                <Td className="hidden font-mono text-xs text-zinc-500 md:table-cell">{p.product_id}</Td>
              </tr>
            ))
          )}
        </tbody>
      </Table>
    </div>
  );
}
