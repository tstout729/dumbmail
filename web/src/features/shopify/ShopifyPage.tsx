import { useState, type FormEvent } from "react";
import { Link, useSearchParams } from "react-router";
import { AlertTriangle, CheckCircle2, CircleDashed, Download, ExternalLink, Pause, Play, RefreshCw, ShoppingBag, Square, Store, XCircle } from "lucide-react";
import { errorMessage, useSession } from "../../api";
import { Badge, Button, buttonClass, Card, ConfirmDialog, EmptyState, Field, Input, Notice, Skeleton } from "../../components/ui";
import { formatDate, formatNumber, formatRelative } from "../../lib/format";
import { toast } from "../../lib/toast";
import { shopifyApi, useConnectShopify, useDisconnectShopify, useResyncShopifyProducts, useShopifyImportAction, useShopifyStatus, useStartShopifyImport } from "./api";
import type { ShopifyImport, ShopifyStatus } from "./types";

/** What the error codes the install callback sends back mean. */
const CALLBACK_ERRORS: Record<string, string> = {
  unavailable: "Shopify isn't set up on this server.",
  signature: "That link wasn't signed by Shopify. Start again from this page.",
  expired: "That install link expired or was already used. Start again from this page.",
  shop: "Shopify sent back a different store than the one you entered. Start again.",
  signin: "Finish connecting in a browser signed in to DumbMail as the person who started it. Sign in, then connect again.",
  browser: "Finish connecting in the same browser you started in. Start again from this page.",
  role: "Only an owner or admin of this account can connect a store.",
  denied: "Shopify didn't approve the connection. Try again, and approve the app in Shopify.",
  shopify: "Shopify didn't answer. Try again in a minute.",
  scopes: "The app needs access to customers, orders and products. Connect again and approve all of it.",
  one_store: "Another store is connected to this account. Disconnect it first: one store per account.",
  taken: "That store is connected to another DumbMail account.",
  failed: "Something went wrong on our side. Please try again.",
};

const PHASES: Record<string, string> = { customers: "Customers", products: "Products", orders: "Orders", done: "Done" };

function safeUrl(u: string | null | undefined): string | null {
  if (!u) return null;
  try {
    const p = new URL(u);
    return p.protocol === "https:" ? p.href : null;
  } catch {
    return null;
  }
}

export function ShopifyPage() {
  const status = useShopifyStatus();
  if (status.isPending) {
    return (
      <div className="space-y-4">
        <Skeleton className="h-40" />
        <Skeleton className="h-56" />
      </div>
    );
  }
  if (status.error) return <p className="text-sm text-red-600">{errorMessage(status.error)}</p>;
  const s = status.data;
  if (!s.available) {
    return (
      <Card title="Shopify">
        <EmptyState
          icon={<Store />}
          title="Shopify isn't available on this server"
          description="Whoever runs this DumbMail server needs to set up a Shopify app first (SHOPIFY_API_KEY and SHOPIFY_API_SECRET). Until then, the website snippet and checkout pixel on the Website tab still track your store."
          action={
            <Link to="/settings/website" className={buttonClass("secondary")}>
              Website snippet
            </Link>
          }
        />
      </Card>
    );
  }
  return (
    <div className="space-y-4">
      <CallbackNotice />
      {s.connected ? <ConnectedCard s={s} /> : <ConnectCard s={s} />}
      {s.connected ? (
        <>
          <HistoryCard s={s} />
          <StorefrontCard s={s} />
        </>
      ) : null}
      {s.data_requests.length > 0 ? <DataRequestsCard s={s} /> : null}
    </div>
  );
}

function CallbackNotice() {
  const [params, setParams] = useSearchParams();
  const error = params.get("error");
  const connected = params.get("connected");
  if (!error && !connected) return null;
  const dismiss = () => {
    const next = new URLSearchParams(params);
    next.delete("error");
    next.delete("connected");
    setParams(next, { replace: true });
  };
  return (
    <div className="space-y-2">
      {connected ? (
        <Notice tone="green" icon={<CheckCircle2 />} title="Your store is connected">
          DumbMail is subscribing to your store's events now. Import your history below to bring in past customers and orders.
        </Notice>
      ) : (
        <Notice tone="red" icon={<XCircle />} title="Your store wasn't connected">
          {CALLBACK_ERRORS[error ?? ""] ?? CALLBACK_ERRORS.failed}
        </Notice>
      )}
      <button type="button" className="text-xs font-medium text-zinc-500 hover:text-zinc-800" onClick={dismiss}>
        Dismiss
      </button>
    </div>
  );
}

function HealthLine({ s }: { s: ShopifyStatus }) {
  const st = s.health.state;
  const tone = st === "server" ? "green" : st === "incomplete" ? "red" : st === "browser_only" || st === "quiet" ? "amber" : "gray";
  const icon = st === "server" ? <CheckCircle2 /> : st === "browser_only" || st === "incomplete" ? <AlertTriangle /> : <CircleDashed />;
  return <Notice tone={tone} icon={icon} title={s.health.message} />;
}

function ConnectCard({ s }: { s: ShopifyStatus }) {
  const [params] = useSearchParams();
  const connect = useConnectShopify();
  const previous = s.connection;
  const onSubmit = (e: FormEvent<HTMLFormElement>) => {
    e.preventDefault();
    const shop = String(new FormData(e.currentTarget).get("shop") ?? "").trim();
    connect.mutate(shop, { onSuccess: (r) => window.location.assign(r.url) });
  };
  return (
    <Card title="Connect your Shopify store">
      <div className="space-y-4">
        <p className="text-sm text-zinc-600">
          Connect your store and every order, refund, shipment and checkout comes straight from Shopify, customers and their email and SMS consent stay in
          step, your products and stock stay current, and you can import years of history. One store per DumbMail account.
        </p>
        <HealthLine s={s} />
        {previous && previous.status !== "connected" ? (
          <Notice tone="gray" icon={<CircleDashed />} title={`${previous.shop} was ${previous.status === "uninstalled" ? "uninstalled in Shopify" : "disconnected"}`}>
            {previous.disconnected_at ? `${formatRelative(previous.disconnected_at)}. ` : null}Profiles, events and products it brought in are still here.
          </Notice>
        ) : null}
        <form onSubmit={onSubmit} className="flex flex-wrap items-end gap-2">
          <Field label="Store address" className="min-w-0 flex-1">
            <Input name="shop" required maxLength={200} defaultValue={params.get("shop") ?? previous?.shop ?? ""} placeholder="your-store.myshopify.com" autoComplete="off" />
          </Field>
          <Button type="submit" variant="primary" icon={<ShoppingBag className="size-3.5" />} loading={connect.isPending}>
            Connect
          </Button>
        </form>
        <p className="text-xs text-zinc-500">Your store's myshopify.com address, or just its name. You'll approve the app in Shopify, then come back here.</p>
      </div>
    </Card>
  );
}

function ConnectedCard({ s }: { s: ShopifyStatus }) {
  const c = s.connection;
  const [confirm, setConfirm] = useState(false);
  const disconnect = useDisconnectShopify();
  if (!c) return null;
  const storeUrl = safeUrl(c.store_url);
  return (
    <Card
      title="Shopify store"
      actions={
        <Button variant="ghost" size="sm" onClick={() => setConfirm(true)}>
          Disconnect
        </Button>
      }
    >
      <div className="space-y-4">
        <div className="flex flex-wrap items-center gap-x-3 gap-y-1">
          <span className="text-base font-semibold text-zinc-950">{c.name ?? c.shop}</span>
          <Badge tone="green" dot>
            Connected
          </Badge>
          <span className="text-sm text-zinc-500">
            {c.shop}
            {storeUrl ? (
              <>
                {" · "}
                <a href={storeUrl} target="_blank" rel="noreferrer" className="inline-flex items-center gap-1 underline">
                  {new URL(storeUrl).host}
                  <ExternalLink className="size-3" />
                </a>
              </>
            ) : null}
          </span>
        </div>
        <HealthLine s={s} />
        {s.problems.map((p) => (
          <Notice key={p} tone="amber" icon={<AlertTriangle />} title={p} />
        ))}
        {c.last_error ? (
          <Notice tone="amber" icon={<AlertTriangle />} title="Something needs attention">
            {c.last_error}
            {c.last_error_at ? ` (${formatRelative(c.last_error_at)})` : null}
          </Notice>
        ) : null}
        <dl className="grid grid-cols-1 gap-x-6 gap-y-3 text-sm sm:grid-cols-3">
          <Fact label="Last webhook" value={c.last_webhook_at ? `${formatRelative(c.last_webhook_at)}${c.last_webhook_topic ? ` · ${c.last_webhook_topic}` : ""}` : "None yet"} />
          <Fact label="Customers linked" value={formatNumber(s.counts.linked_customers)} />
          <Fact label="Products from Shopify" value={formatNumber(s.counts.products)} />
        </dl>
        <div>
          <h3 className="mb-1.5 text-sm font-medium text-zinc-900">What syncs</h3>
          <ul className="grid gap-1 text-sm text-zinc-600 sm:grid-cols-2">
            {s.syncing.map((x) => (
              <li key={x} className="flex items-start gap-2">
                <CheckCircle2 className="mt-0.5 size-3.5 shrink-0 text-accent-700" />
                {x}
              </li>
            ))}
          </ul>
        </div>
        <p className="text-xs text-zinc-500">
          Connected {formatRelative(c.connected_at)}
          {c.token_hint ? ` with access ${c.token_hint}` : null}.
        </p>
      </div>
      {confirm ? (
        <ConfirmDialog
          title={`Disconnect ${c.shop}?`}
          body="DumbMail removes its webhooks and checkout pixel from the store and forgets its access. Profiles, events and products already brought in stay. You can connect it again any time."
          confirmLabel="Disconnect"
          danger
          loading={disconnect.isPending}
          onClose={() => setConfirm(false)}
          onConfirm={() =>
            disconnect.mutate(undefined, {
              onSuccess: (r) => {
                setConfirm(false);
                toast.success(r.note);
              },
            })
          }
        />
      ) : null}
    </Card>
  );
}

function Fact({ label, value }: { label: string; value: string }) {
  return (
    <div className="min-w-0">
      <dt className="text-xs text-zinc-500">{label}</dt>
      <dd className="truncate font-medium text-zinc-900">{value}</dd>
    </div>
  );
}

function ImportProgress({ i, tz }: { i: ShopifyImport; tz: string }) {
  const counts = [
    i.counts.customers != null ? `${formatNumber(i.counts.customers)} customers` : null,
    i.counts.products != null ? `${formatNumber(i.counts.products)} products` : null,
    i.counts.orders != null ? `${formatNumber(i.counts.orders)} orders` : null,
  ].filter(Boolean);
  const label: Record<string, string> = { running: "Running", paused: "Paused", cancelled: "Cancelled", done: "Finished", failed: "Stopped" };
  const tone = i.status === "done" ? "green" : i.status === "failed" ? "red" : i.status === "running" ? "blue" : "gray";
  return (
    <div className="space-y-2">
      <div className="flex flex-wrap items-center gap-2 text-sm">
        <Badge tone={tone} dot>
          {label[i.status] ?? i.status}
        </Badge>
        {i.status === "running" || i.status === "paused" ? (
          <span className="text-zinc-600">
            Step {Math.max(1, i.phases.indexOf(i.phase) + 1)} of {i.phases.length}: {PHASES[i.phase] ?? i.phase}
            {i.shopify_status === "running" || i.shopify_status === "created" ? " (Shopify is preparing it)" : null}
          </span>
        ) : (
          <span className="text-zinc-600">{i.finished_at ? formatRelative(i.finished_at) : null}</span>
        )}
      </div>
      {i.phases.length > 1 ? (
        <ol className="flex gap-1.5" aria-label="Import steps">
          {i.phases.map((p) => {
            const at = i.phases.indexOf(i.phase);
            const idx = i.phases.indexOf(p);
            const done = i.phase === "done" || idx < at;
            return <li key={p} className={`h-1.5 flex-1 rounded-full ${done ? "bg-accent-600" : idx === at ? "bg-accent-300" : "bg-zinc-200"}`} title={PHASES[p]} />;
          })}
        </ol>
      ) : null}
      {counts.length ? <p className="text-sm text-zinc-600">Brought in so far: {counts.join(" · ")}.</p> : null}
      {i.kind === "history" ? <p className="text-xs text-zinc-500">Orders since {formatDate(i.orders_since, tz)}.</p> : null}
      {i.error ? <p className="text-sm text-red-600">{i.error}</p> : null}
    </div>
  );
}

function HistoryCard({ s }: { s: ShopifyStatus }) {
  const { account } = useSession();
  const [confirm, setConfirm] = useState(false);
  const start = useStartShopifyImport();
  const action = useShopifyImportAction();
  const resync = useResyncShopifyProducts();
  const i = s.import;
  const active = i?.status === "running" || i?.status === "paused";
  const productsBusy = s.products_sync?.status === "running";
  return (
    <Card title="History">
      <div className="space-y-4">
        <p className="text-sm text-zinc-600">
          Bring in every customer and product, and the last 3 years of orders with their refunds and shipments. Customers subscribed in Shopify are
          subscribed here; nobody else is. Imported orders don't start flows or send anything, and running it again adds nothing twice.
        </p>
        {i ? <ImportProgress i={i} tz={account.timezone} /> : null}
        <div className="flex flex-wrap gap-2">
          {!active ? (
            <Button variant={i ? "secondary" : "primary"} disabled={productsBusy} onClick={() => setConfirm(true)}>
              {i ? "Import again" : "Import history"}
            </Button>
          ) : null}
          {i?.status === "running" ? (
            <Button icon={<Pause className="size-3.5" />} loading={action.isPending} onClick={() => action.mutate("pause")}>
              Pause
            </Button>
          ) : null}
          {i?.status === "paused" ? (
            <Button icon={<Play className="size-3.5" />} loading={action.isPending} onClick={() => action.mutate("resume")}>
              Resume
            </Button>
          ) : null}
          {active ? (
            <Button variant="ghost" icon={<Square className="size-3.5" />} onClick={() => action.mutate("cancel")}>
              Cancel
            </Button>
          ) : null}
        </div>
        <div className="border-t border-zinc-100 pt-4">
          <div className="flex flex-wrap items-center justify-between gap-2">
            <div className="text-sm">
              <div className="font-medium text-zinc-900">Products</div>
              <div className="text-zinc-600">
                {productsBusy
                  ? "Reading every product from Shopify now…"
                  : s.products_sync?.finished_at
                    ? `Fully checked ${formatRelative(s.products_sync.finished_at)}. Changes arrive as they happen in between.`
                    : "Changes arrive as they happen, and every product is checked again daily."}
              </div>
            </div>
            <Button size="sm" icon={<RefreshCw className="size-3.5" />} disabled={productsBusy || active} loading={resync.isPending} onClick={() => resync.mutate(undefined)}>
              Sync products now
            </Button>
          </div>
        </div>
      </div>
      {confirm ? (
        <ConfirmDialog
          title="Import your Shopify history?"
          body="DumbMail reads every customer and product, and orders from the last 3 years, in the background. It can take a while for a big store; you can pause or cancel it. Nothing is sent and no flow starts."
          confirmLabel="Start import"
          loading={start.isPending}
          onClose={() => setConfirm(false)}
          onConfirm={() => start.mutate(undefined, { onSuccess: () => setConfirm(false) })}
        />
      ) : null}
    </Card>
  );
}

function StorefrontCard({ s }: { s: ShopifyStatus }) {
  const embed = safeUrl(s.storefront.embed_link);
  return (
    <Card title="On your storefront">
      <div className="space-y-3">
        <Notice tone={s.storefront.pixel_installed ? "green" : "gray"} icon={s.storefront.pixel_installed ? <CheckCircle2 /> : <CircleDashed />} title={s.storefront.pixel_installed ? "Checkout pixel installed" : "Checkout pixel not installed"}>
          {s.storefront.pixel_installed
            ? "It reports checkouts and orders from Shopify's checkout pages and tells your site who checked out."
            : "Add the custom pixel by hand from the Website tab, or connect again once the app's extensions are deployed."}
        </Notice>
        <div className="flex flex-wrap items-center justify-between gap-2 rounded-lg border border-zinc-200 px-3.5 py-3">
          <div className="min-w-0 text-sm">
            <div className="font-medium text-zinc-900">Website snippet on every page</div>
            <div className="text-zinc-600">Turn on the DumbMail app embed in your theme, then save. You don't need to paste the snippet as well.</div>
          </div>
          {embed ? (
            <a href={embed} target="_blank" rel="noreferrer" className={buttonClass("secondary", "sm")}>
              Open theme editor <ExternalLink className="size-3.5" />
            </a>
          ) : null}
        </div>
        {s.storefront.error ? <p className="text-sm text-amber-800">{s.storefront.error}</p> : null}
        <p className="text-xs text-zinc-500">
          The pasted snippet and custom pixel on the <Link to="/settings/website" className="font-medium underline">Website tab</Link> keep working too.
        </p>
      </div>
    </Card>
  );
}

function DataRequestsCard({ s }: { s: ShopifyStatus }) {
  const { account } = useSession();
  const [loading, setLoading] = useState<string | null>(null);
  const download = async (id: string) => {
    setLoading(id);
    try {
      const data = await shopifyApi.dataRequest(id);
      const url = URL.createObjectURL(new Blob([JSON.stringify(data, null, 2)], { type: "application/json" }));
      const a = document.createElement("a");
      a.href = url;
      a.download = `shopify-data-request-${id}.json`;
      a.click();
      URL.revokeObjectURL(url);
    } catch (e) {
      toast.error(errorMessage(e));
    } finally {
      setLoading(null);
    }
  };
  return (
    <Card title="Data requests">
      <div className="space-y-3">
        <p className="text-sm text-zinc-600">
          A customer asked your store for the data held about them. Download what DumbMail holds and send it to them; Shopify expects an answer within 30
          days. Exports are kept for 90 days.
        </p>
        <ul className="divide-y divide-zinc-100 rounded-lg border border-zinc-200">
          {s.data_requests.map((r) => (
            <li key={r.id} className="flex flex-wrap items-center justify-between gap-2 px-3.5 py-2.5 text-sm">
              <div className="min-w-0">
                <div className="truncate font-medium text-zinc-900">{r.shopify_customer_id ? `Shopify customer ${r.shopify_customer_id}` : "A customer"}</div>
                <div className="text-xs text-zinc-500">
                  Asked {formatDate(r.created_at, account.timezone)}
                  {r.downloaded_at ? ` · downloaded ${formatRelative(r.downloaded_at)}` : " · not downloaded yet"}
                </div>
              </div>
              <Button size="sm" icon={<Download className="size-3.5" />} loading={loading === r.id} onClick={() => void download(r.id)}>
                Download
              </Button>
            </li>
          ))}
        </ul>
      </div>
    </Card>
  );
}
