import { useState, type FormEvent } from "react";
import { useSearchParams } from "react-router";
import { Plus, RefreshCw, Rss, Trash2, X } from "lucide-react";
import { useSession } from "../../api";
import { Badge, Button, Card, Checkbox, ConfirmDialog, EmptyState, Field, IconButton, Input, Modal, Page, PageHeader, Select, Skeleton } from "../../components/ui";
import { formatDateTime, formatNumber, formatRelative } from "../../lib/format";
import { toast } from "../../lib/toast";
import { useCreateFeed, useDeleteFeed, useFeeds, useSyncFeed, useUpdateFeed } from "./api";
import { CatalogTabs } from "./parts";
import type { Feed, FeedFormat, FeedInput } from "./types";

const mapFields: { value: string; label: string }[] = [
  { value: "items_path", label: "List of products (JSON path)" },
  { value: "external_id", label: "Product id" },
  { value: "title", label: "Title" },
  { value: "description", label: "Description" },
  { value: "url", label: "Product link" },
  { value: "image_url", label: "Image" },
  { value: "images", label: "More images" },
  { value: "price", label: "Price" },
  { value: "sale_price", label: "Sale price" },
  { value: "compare_at_price", label: "Compare-at price" },
  { value: "currency", label: "Currency" },
  { value: "available", label: "Availability" },
  { value: "inventory", label: "Inventory" },
  { value: "categories", label: "Categories" },
  { value: "tags", label: "Tags" },
  { value: "brand", label: "Brand" },
  { value: "vendor", label: "Vendor" },
  { value: "published", label: "Published" },
  { value: "group_id", label: "Variant group (XML)" },
  { value: "variant_title", label: "Variant title (XML)" },
  { value: "sku", label: "SKU (XML)" },
];

const intervals = [1, 3, 6, 12, 24, 48, 168];

function statusBadge(f: Feed) {
  if (f.syncing) return <Badge tone="blue" dot>Syncing</Badge>;
  if (!f.enabled) return <Badge tone="gray" dot>Off</Badge>;
  if (f.last_status === "error") return <Badge tone="red" dot>Error</Badge>;
  if (f.last_status === "ok") return <Badge tone="green" dot>Synced</Badge>;
  if (f.last_status === "not_modified") return <Badge tone="green" dot>Up to date</Badge>;
  return <Badge tone="gray" dot>Waiting</Badge>;
}

export function FeedsPage() {
  const { account } = useSession();
  const [params, setParams] = useSearchParams();
  const feeds = useFeeds();
  const sync = useSyncFeed();
  const remove = useDeleteFeed();
  const [editing, setEditing] = useState<Feed | null>(null);
  const [deleting, setDeleting] = useState<Feed | null>(null);
  const adding = params.get("add") === "1";
  const closeAdd = () => {
    const next = new URLSearchParams(params);
    next.delete("add");
    setParams(next, { replace: true });
  };

  return (
    <Page>
      <PageHeader
        title="Products"
        description="Feeds keep your catalog up to date on a schedule."
        actions={
          <Button variant="primary" icon={<Plus className="size-3.5" />} onClick={() => setParams({ add: "1" }, { replace: true })}>
            Add a feed
          </Button>
        }
      />
      <CatalogTabs />
      {feeds.isPending ? (
        <Skeleton className="h-32 w-full" />
      ) : feeds.error ? (
        <Card>
          <EmptyState title="Couldn't load feeds" description={feeds.error.message} />
        </Card>
      ) : (feeds.data ?? []).length === 0 ? (
        <Card>
          <EmptyState
            icon={<Rss className="size-5" />}
            title="No feeds yet"
            description="Add your store's product feed: a Google Shopping (Merchant Center) XML feed, or any JSON list of products. Shopify stores can use https://your-store.com/products.json."
            action={
              <Button variant="primary" onClick={() => setParams({ add: "1" }, { replace: true })}>
                Add a feed
              </Button>
            }
          />
        </Card>
      ) : (
        <div className="space-y-3">
          {(feeds.data ?? []).map((f) => (
            <Card key={f.id}>
              <div className="flex flex-wrap items-start justify-between gap-3">
                <div className="min-w-0">
                  <div className="flex flex-wrap items-center gap-2">
                    <h2 className="text-sm font-semibold text-zinc-900">{f.name}</h2>
                    {statusBadge(f)}
                    <Badge>{f.format === "google_xml" ? "Google Shopping XML" : "JSON"}</Badge>
                  </div>
                  <p className="mt-1 truncate text-xs text-zinc-500">{f.url}</p>
                </div>
                <div className="flex items-center gap-1">
                  <Button
                    size="sm"
                    icon={<RefreshCw className="size-3.5" />}
                    loading={sync.isPending && sync.variables === f.id}
                    disabled={f.syncing}
                    onClick={() => sync.mutate(f.id, { onSuccess: (r) => toast.success(r.queued ? "Sync started" : "A sync is already waiting") })}
                  >
                    Sync now
                  </Button>
                  <Button size="sm" variant="ghost" onClick={() => setEditing(f)}>
                    Edit
                  </Button>
                  <IconButton label="Delete feed" onClick={() => setDeleting(f)}>
                    <Trash2 className="size-4" />
                  </IconButton>
                </div>
              </div>
              <dl className="mt-3 grid grid-cols-2 gap-3 text-sm sm:grid-cols-4">
                <div>
                  <dt className="text-xs text-zinc-500">Products</dt>
                  <dd className="tabular-nums text-zinc-900">{formatNumber(f.products ?? 0)}</dd>
                </div>
                <div>
                  <dt className="text-xs text-zinc-500">Last sync</dt>
                  <dd className="text-zinc-900" title={formatDateTime(f.last_synced_at, account.timezone)}>
                    {f.last_synced_at ? formatRelative(f.last_synced_at) : "Not yet"}
                  </dd>
                </div>
                <div>
                  <dt className="text-xs text-zinc-500">Last run</dt>
                  <dd className="text-zinc-900">
                    {f.last_counts.fetched !== undefined
                      ? `${formatNumber(f.last_counts.created ?? 0)} new, ${formatNumber(f.last_counts.updated ?? 0)} changed${f.last_counts.unpublished ? `, ${formatNumber(f.last_counts.unpublished)} removed` : ""}`
                      : "—"}
                  </dd>
                </div>
                <div>
                  <dt className="text-xs text-zinc-500">Schedule</dt>
                  <dd className="text-zinc-900">{f.enabled ? `Every ${f.interval_hours === 1 ? "hour" : `${f.interval_hours} hours`}` : "Off"}</dd>
                </div>
              </dl>
              {f.last_error ? (
                <p className={f.last_status === "error" ? "mt-3 rounded-md bg-red-50 px-3 py-2 text-xs text-red-700" : "mt-3 rounded-md bg-amber-50 px-3 py-2 text-xs text-amber-800"}>{f.last_error}</p>
              ) : null}
            </Card>
          ))}
        </div>
      )}
      {adding ? <FeedDialog onClose={closeAdd} /> : null}
      {editing ? <FeedDialog feed={editing} onClose={() => setEditing(null)} /> : null}
      {deleting ? (
        <ConfirmDialog
          title="Delete this feed?"
          body="Its products stay in your catalog but stop updating."
          confirmLabel="Delete feed"
          danger
          loading={remove.isPending}
          onConfirm={() => remove.mutate(deleting.id, { onSuccess: () => setDeleting(null) })}
          onClose={() => setDeleting(null)}
        />
      ) : null}
    </Page>
  );
}

type MapRow = { field: string; path: string };

function FeedDialog({ feed, onClose }: { feed?: Feed; onClose: () => void }) {
  const create = useCreateFeed();
  const update = useUpdateFeed(feed?.id ?? "");
  const [format, setFormat] = useState<FeedFormat>(feed?.format ?? "google_xml");
  const [rows, setRows] = useState<MapRow[]>(() => Object.entries(feed?.field_map ?? {}).map(([field, path]) => ({ field, path })));
  const busy = create.isPending || update.isPending;

  const onSubmit = (e: FormEvent<HTMLFormElement>) => {
    e.preventDefault();
    const d = new FormData(e.currentTarget);
    const field_map = Object.fromEntries(rows.filter((r) => r.field && r.path.trim()).map((r) => [r.field, r.path.trim()]));
    const input: FeedInput = {
      name: String(d.get("name") ?? "").trim() || undefined,
      url: String(d.get("url") ?? "").trim(),
      format,
      interval_hours: Number(d.get("interval_hours")),
      enabled: d.get("enabled") === "on",
      unpublish_missing: d.get("unpublish_missing") === "on",
      field_map,
    };
    const done = () => {
      toast.success(feed ? "Feed saved" : "Feed added. The first sync is running.");
      onClose();
    };
    if (feed) update.mutate(input, { onSuccess: done });
    else create.mutate(input, { onSuccess: done });
  };

  return (
    <Modal
      title={feed ? "Edit feed" : "Add a product feed"}
      description="DumbMail fetches the feed on a schedule and only downloads it again when it changed."
      onClose={onClose}
      size="lg"
    >
      <form onSubmit={onSubmit} className="space-y-4">
        <div className="grid grid-cols-1 gap-3 sm:grid-cols-2">
          <Field label="Feed address" className="sm:col-span-2" hint="A public http(s) address.">
            <Input name="url" type="url" required defaultValue={feed?.url} placeholder="https://your-store.com/feed.xml" autoFocus />
          </Field>
          <Field label="Name">
            <Input name="name" defaultValue={feed?.name} placeholder="Product feed" />
          </Field>
          <Field label="Format">
            <Select value={format} onChange={(e) => setFormat(e.target.value as FeedFormat)}>
              <option value="google_xml">Google Shopping XML (RSS or Atom)</option>
              <option value="json">JSON (including Shopify products.json)</option>
            </Select>
          </Field>
          <Field label="Sync">
            <Select name="interval_hours" defaultValue={feed?.interval_hours ?? 6}>
              {intervals.map((h) => (
                <option key={h} value={h}>
                  {h === 1 ? "Every hour" : h === 168 ? "Once a week" : `Every ${h} hours`}
                </option>
              ))}
            </Select>
          </Field>
          <div className="space-y-2 pt-6">
            <Checkbox name="enabled" defaultChecked={feed?.enabled ?? true} label="Sync on this schedule" />
            <Checkbox name="unpublish_missing" defaultChecked={feed?.unpublish_missing ?? true} label="Unpublish products that leave the feed" />
          </div>
        </div>
        <div>
          <div className="mb-1 text-sm font-medium text-zinc-900">Field map</div>
          <p className="mb-2 text-xs text-zinc-500">
            Only needed when the defaults don't find a field.{" "}
            {format === "json" ? "Use a dot path such as data.items or pricing.current." : "Use an element name such as g:custom_label_1."} Map to custom.name to keep any other field.
          </p>
          <div className="space-y-2">
            {rows.map((r, i) => (
              <div key={i} className="flex items-center gap-2">
                <Input
                  value={r.field}
                  onChange={(e) => setRows(rows.map((x, j) => (j === i ? { ...x, field: e.target.value } : x)))}
                  list="dm-feed-fields"
                  placeholder="Our field"
                  aria-label="Our field"
                  className="w-48"
                />
                <Input value={r.path} onChange={(e) => setRows(rows.map((x, j) => (j === i ? { ...x, path: e.target.value } : x)))} placeholder="The feed's field" aria-label="The feed's field" />
                <IconButton label="Remove" onClick={() => setRows(rows.filter((_, j) => j !== i))}>
                  <X className="size-4" />
                </IconButton>
              </div>
            ))}
            <datalist id="dm-feed-fields">
              {mapFields.map((f) => (
                <option key={f.value} value={f.value}>
                  {f.label}
                </option>
              ))}
            </datalist>
            <button type="button" onClick={() => setRows([...rows, { field: "", path: "" }])} className="text-xs font-medium text-accent-700 hover:underline">
              + Map a field
            </button>
          </div>
        </div>
        <div className="flex justify-end gap-2 border-t border-zinc-100 pt-4">
          <Button variant="ghost" onClick={onClose}>
            Cancel
          </Button>
          <Button type="submit" variant="primary" loading={busy}>
            {feed ? "Save" : "Add feed"}
          </Button>
        </div>
      </form>
    </Modal>
  );
}
