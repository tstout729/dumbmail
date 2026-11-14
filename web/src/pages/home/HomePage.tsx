import { Link, useSearchParams } from "react-router";
import { ArrowRight, Check, Circle, X } from "lucide-react";
import { useOverview, useSending, useSession, useUpdateAccount, useWebsite } from "../../api";
import { KpiRow, RevenueChart, SendsChart } from "../../components/analytics/OverviewParts";
import { RangePicker, parseRange } from "../../components/charts/RangePicker";
import { Card, EmptyState, IconButton, Page, PageHeader, Skeleton, StatusBadge, cx } from "../../components/ui";
import { formatCurrency, formatDate, formatNumber, formatPercent } from "../../lib/format";

function SetupChecklist() {
  const { account } = useSession();
  const sending = useSending();
  const website = useWebsite();
  const update = useUpdateAccount();
  const o = account.onboarding;
  const items = [
    { done: o.brand, label: "Add your brand details", detail: "Website, colors and mailing address", to: "/settings" },
    { done: o.sender || Boolean(account.from_email), label: "Choose a sender email", detail: "The address your emails come from", to: "/settings/sending" },
    { done: sending.data?.email.status === "verified", label: "Verify your sending domain", detail: "Add DNS records so you land in the inbox", to: "/settings/domains" },
    { done: website.data?.installed ?? false, label: "Add the snippet to your website", detail: "Tracks visitors and shows your sign-up forms", to: "/settings/website" },
    { done: o.contacts, label: "Import contacts", detail: "Paste or upload a CSV", to: "/audience/profiles?import=1" },
    { done: o.first_flow, label: "Turn on your first flow", detail: "A welcome series is a great start", to: "/flows?create=1" },
  ];
  const doneCount = items.filter((i) => i.done).length;

  return (
    <Card className="mb-6" padded={false}>
      <div className="flex items-start justify-between gap-3 px-5 pt-4">
        <div>
          <h2 className="text-sm font-semibold text-zinc-900">Get set up</h2>
          <p className="text-xs text-zinc-500">
            {doneCount} of {items.length} done
          </p>
        </div>
        <IconButton label="Dismiss checklist" onClick={() => update.mutate({ onboarding: { dismissed: true } })}>
          <X className="size-4" />
        </IconButton>
      </div>
      <div className="mx-5 mt-3 h-1 overflow-hidden rounded-full bg-zinc-100">
        <div className="h-full bg-accent-600 transition-[width]" style={{ width: `${(doneCount / items.length) * 100}%` }} />
      </div>
      <ul className="mt-2 divide-y divide-zinc-100">
        {items.map((item) => (
          <li key={item.label}>
            <Link to={item.to} className="group flex items-center gap-3 px-5 py-2.5 hover:bg-zinc-50">
              {item.done ? (
                <span className="flex size-5 items-center justify-center rounded-full bg-accent-600 text-white">
                  <Check className="size-3" />
                </span>
              ) : (
                <Circle className="size-5 text-zinc-300" />
              )}
              <span className="min-w-0 flex-1">
                <span className={cx("block text-sm font-medium", item.done ? "text-zinc-400 line-through" : "text-zinc-900")}>{item.label}</span>
                <span className="block text-xs text-zinc-500">{item.detail}</span>
              </span>
              <ArrowRight className="size-4 text-zinc-300 group-hover:text-zinc-600" />
            </Link>
          </li>
        ))}
      </ul>
    </Card>
  );
}

export function HomePage() {
  const { account, user } = useSession();
  const [params, setParams] = useSearchParams();
  const days = parseRange(params.get("days"));
  const overview = useOverview(days);
  const loading = overview.isPending;
  const data = overview.data;
  const recent = [...(data?.top_campaigns ?? [])].sort((a, b) => (b.sent_at ?? "").localeCompare(a.sent_at ?? "")).slice(0, 5);
  const topFlows = [...(data?.top_flows ?? [])].sort((a, b) => b.revenue - a.revenue).slice(0, 5);
  const tz = account.timezone;

  return (
    <Page>
      <PageHeader
        title={`Welcome back${user.name ? `, ${user.name.split(" ")[0]}` : ""}`}
        description={account.name}
        actions={<RangePicker value={days} onChange={(d) => setParams({ days: String(d) }, { replace: true })} />}
      />
      {!account.onboarding.dismissed ? <SetupChecklist /> : null}
      {overview.error ? <p className="mb-4 text-sm text-red-600">{overview.error.message}</p> : null}
      <KpiRow data={data} loading={loading} />
      <div className="mt-4 grid gap-4 lg:grid-cols-2">
        <RevenueChart data={data} loading={loading} />
        <SendsChart data={data} loading={loading} />
      </div>
      <div className="mt-4 grid gap-4 lg:grid-cols-2">
        <Card title="Recent campaigns" actions={<Link to="/campaigns" className="text-xs font-medium text-accent-700 hover:underline">View all</Link>} padded={false}>
          {loading ? (
            <ListSkeleton />
          ) : recent.length === 0 ? (
            <EmptyState title="No campaigns sent yet" description="Your sent campaigns show up here with opens, clicks and revenue." />
          ) : (
            <ul className="divide-y divide-zinc-100">
              {recent.map((c) => (
                <li key={c.id}>
                  <Link to={`/campaigns/${c.id}`} className="flex items-center gap-3 px-4 py-2.5 hover:bg-zinc-50">
                    <span className="min-w-0 flex-1">
                      <span className="block truncate text-sm font-medium text-zinc-900">{c.name}</span>
                      <span className="block text-xs text-zinc-500">
                        {formatDate(c.sent_at, tz)} · {formatNumber(c.recipients)} recipients
                      </span>
                    </span>
                    <span className="text-right text-xs tabular-nums text-zinc-600">
                      <span className="block">{formatPercent(c.open_rate)} open</span>
                      <span className="block">{formatPercent(c.click_rate)} click</span>
                    </span>
                    <span className="w-20 text-right text-sm font-medium tabular-nums">{formatCurrency(c.revenue)}</span>
                  </Link>
                </li>
              ))}
            </ul>
          )}
        </Card>
        <Card title="Top flows" actions={<Link to="/flows" className="text-xs font-medium text-accent-700 hover:underline">View all</Link>} padded={false}>
          {loading ? (
            <ListSkeleton />
          ) : topFlows.length === 0 ? (
            <EmptyState title="No flows yet" description="Automations like welcome series and abandoned cart do the heavy lifting." />
          ) : (
            <ul className="divide-y divide-zinc-100">
              {topFlows.map((f) => (
                <li key={f.id}>
                  <Link to={`/flows/${f.id}`} className="flex items-center gap-3 px-4 py-2.5 hover:bg-zinc-50">
                    <span className="min-w-0 flex-1">
                      <span className="block truncate text-sm font-medium text-zinc-900">{f.name}</span>
                      <span className="block text-xs text-zinc-500">{formatNumber(f.recipients)} recipients</span>
                    </span>
                    <StatusBadge status={f.status} />
                    <span className="w-20 text-right text-sm font-medium tabular-nums">{formatCurrency(f.revenue)}</span>
                  </Link>
                </li>
              ))}
            </ul>
          )}
        </Card>
      </div>
    </Page>
  );
}

function ListSkeleton() {
  return (
    <div className="space-y-3 p-4">
      {[0, 1, 2].map((i) => (
        <Skeleton key={i} className="h-9 w-full" />
      ))}
    </div>
  );
}
