import { useState, type ReactNode } from "react";
import { Link, useSearchParams } from "react-router";
import { AlertTriangle, ArrowRight, CheckCircle2, FileText, Info, Mail, OctagonAlert, Workflow } from "lucide-react";
import { useOverview, useSession, type AnalyticsOverview, type OverviewTotals } from "../../api";
import { DeltaBadge, Hint, KpiTile, LegendDot, SplitBar, change } from "../../components/analytics/kit";
import { BarSeriesChart } from "../../components/charts/BarSeriesChart";
import { RangePicker, parseRange } from "../../components/charts/RangePicker";
import { TimeSeriesChart } from "../../components/charts/TimeSeriesChart";
import { Card, EmptyRow, Page, PageHeader, Segmented, Skeleton, SkeletonRows, StatusBadge, Table, Td, Th, cx } from "../../components/ui";
import { useAttribution } from "../../features/reports/api";
import { formatCompact, formatCurrency, formatDate, formatNumber, formatPercent, pluralize } from "../../lib/format";

const color = { flows: "#219464", campaigns: "#6d5bd0", other: "#d4d4d8", email: "#219464", sms: "#6d5bd0" };

type Series = AnalyticsOverview["series"];

/** Weekly sums for sparklines, so a quiet day doesn't read as a crash. Daily for short ranges. */
function trendOf(series: Series, pick: (d: Series[number]) => number): number[] {
  const values = series.map(pick);
  if (values.length <= 14) return values;
  const size = Math.ceil(values.length / 12);
  const out: number[] = [];
  for (let i = values.length; i > 0; i -= size) out.unshift(values.slice(Math.max(0, i - size), i).reduce((a, b) => a + b, 0));
  return out;
}

/** The same buckets as `trendOf`, as a rate (e.g. opens per email sent). */
function rateTrendOf(series: Series, num: (d: Series[number]) => number, den: (d: Series[number]) => number): number[] {
  const n = trendOf(series, num);
  const d = trendOf(series, den);
  return n.map((v, i) => (d[i] ? v / d[i] : 0));
}

function hasActivity(t: OverviewTotals | undefined) {
  return Boolean(t && (t.emails_sent || t.sms_sent || t.revenue || t.orders || t.new_profiles));
}

export function AnalyticsPage() {
  const [params, setParams] = useSearchParams();
  const days = parseRange(params.get("days"));
  const overview = useOverview(days);
  const data = overview.data;
  const loading = overview.isPending;
  const vs = `vs previous ${days} days`;
  const empty = !loading && data && !hasActivity(data.totals) && !hasActivity(data.previous);

  return (
    <Page>
      <PageHeader
        title="Overview"
        description={`What your emails and texts did in the last ${days} days, compared with the ${days} days before.`}
        actions={<RangePicker value={days} onChange={(d) => setParams({ days: String(d) }, { replace: true })} />}
      />
      {overview.error ? <p className="mb-4 text-sm text-red-600">{overview.error.message}</p> : null}
      {empty ? (
        <GetStarted days={days} />
      ) : (
        <div className={cx("space-y-4 transition-opacity", overview.isPlaceholderData && "opacity-60")}>
          <RevenueHero data={data} loading={loading} vs={vs} />
          <KpiRow data={data} loading={loading} vs={vs} />
          <div className="grid grid-cols-1 gap-4 lg:grid-cols-2">
            <Funnel data={data} loading={loading} />
            <Health data={data} loading={loading} vs={vs} />
          </div>
          <Activity data={data} loading={loading} />
          <CampaignsTable data={data} loading={loading} />
          <FlowsTable data={data} loading={loading} />
        </div>
      )}
    </Page>
  );
}

function RevenueHero({ data, loading, vs }: { data: AnalyticsOverview | undefined; loading: boolean; vs: string }) {
  const attribution = useAttribution();
  const t = data?.totals;
  const p = data?.previous;
  const share = t && t.revenue > 0 ? t.attributed_revenue / t.revenue : 0;
  const rows = (data?.series ?? []).map((d) => ({
    date: d.date,
    flows: d.flow_revenue,
    campaigns: d.campaign_revenue,
    other: Math.max(0, d.revenue - d.attributed_revenue),
  }));
  const sources = [
    { key: "flows", label: "Flows", value: t?.flow_revenue ?? 0, color: color.flows, prev: p?.flow_revenue ?? 0, to: "/flows" },
    { key: "campaigns", label: "Campaigns", value: t?.campaign_revenue ?? 0, color: color.campaigns, prev: p?.campaign_revenue ?? 0, to: "/campaigns" },
  ];

  return (
    <Card padded={false}>
      <div className="grid grid-cols-1 lg:grid-cols-[minmax(0,320px)_minmax(0,1fr)]">
        <div className="flex flex-col gap-5 border-b border-zinc-100 p-5 lg:border-r lg:border-b-0">
          <div>
            <div className="flex items-center gap-1 text-sm font-medium text-zinc-600">
              Revenue from DumbMail
              <Hint text={`${attribution.data?.description ?? "An order counts when the buyer opened or clicked one of your emails in the 5 days before, or clicked a text in the 24 hours before. The newest message gets the credit."} Change this in Settings → Attribution.`} />
            </div>
            {loading ? (
              <Skeleton className="mt-2 h-12 w-40" />
            ) : (
              <div className="mt-1 flex flex-wrap items-center gap-x-2.5 gap-y-1">
                <span className="text-5xl font-semibold tracking-tight text-zinc-950">{formatCurrency(t?.attributed_revenue)}</span>
                <DeltaBadge value={change(t?.attributed_revenue ?? 0, p?.attributed_revenue ?? 0, "count")} label={vs} />
              </div>
            )}
            {loading ? (
              <Skeleton className="mt-2 h-4 w-52" />
            ) : (
              <p className="mt-1.5 text-sm text-zinc-500">
                {pluralize(t?.attributed_orders ?? 0, "order")} ·{" "}
                {t && t.revenue > 0 ? (
                  <>
                    <span className="font-medium text-zinc-700">{formatPercent(share, 0)}</span> of {formatCurrency(t.revenue)} store revenue
                  </>
                ) : (
                  "no store orders tracked yet"
                )}
              </p>
            )}
          </div>
          <div>
            <SplitBar parts={sources} />
            <ul className="mt-3 space-y-2">
              {sources.map((s) => (
                <li key={s.key}>
                  <Link to={s.to} className="group -mx-2 flex items-center justify-between gap-2 rounded-md px-2 py-1 hover:bg-zinc-50">
                    <LegendDot color={s.color}>
                      <span className="text-sm text-zinc-700 group-hover:text-zinc-950">{s.label}</span>
                    </LegendDot>
                    <span className="flex items-center gap-2">
                      <DeltaBadge value={change(s.value, s.prev, "count")} label={vs} />
                      <span className="text-sm font-medium tabular-nums text-zinc-900">{loading ? "—" : formatCurrency(s.value)}</span>
                    </span>
                  </Link>
                </li>
              ))}
            </ul>
          </div>
        </div>
        <div className="min-w-0 p-5">
          <div className="mb-3 flex flex-wrap items-center justify-between gap-2">
            <h2 className="text-sm font-semibold text-zinc-900">Revenue by day</h2>
            <div className="flex flex-wrap gap-3">
              <LegendDot color={color.flows}>Flows</LegendDot>
              <LegendDot color={color.campaigns}>Campaigns</LegendDot>
              <LegendDot color={color.other}>Other store revenue</LegendDot>
            </div>
          </div>
          <BarSeriesChart
            loading={loading}
            data={rows}
            xKey="date"
            height={232}
            showTotal
            format={(n) => (n >= 1000 ? `$${formatCompact(n)}` : formatCurrency(n))}
            series={[
              { key: "flows", label: "Flows", color: color.flows },
              { key: "campaigns", label: "Campaigns", color: color.campaigns },
              { key: "other", label: "Other store revenue", color: color.other },
            ]}
          />
        </div>
      </div>
    </Card>
  );
}

function KpiRow({ data, loading, vs }: { data: AnalyticsOverview | undefined; loading: boolean; vs: string }) {
  const t = data?.totals;
  const p = data?.previous;
  const s = data?.series ?? [];
  const sent = (t?.emails_sent ?? 0) + (t?.sms_sent ?? 0);
  const prevSent = (p?.emails_sent ?? 0) + (p?.sms_sent ?? 0);
  // A rate against a period with no sends isn't a change, it's a first.
  const rateChange = (cur: number, prev: number, hadSends: boolean) => (hadSends ? change(cur, prev, "rate") : null);
  return (
    <div className="grid grid-cols-2 gap-3 lg:grid-cols-4">
      <KpiTile
        label="Messages sent"
        value={formatNumber(sent)}
        change={change(sent, prevSent, "count")}
        compareLabel={vs}
        sub={`${formatNumber(t?.emails_sent)} email · ${formatNumber(t?.sms_sent)} SMS`}
        trend={trendOf(s, (d) => d.emails_sent + d.sms_sent)}
        loading={loading}
      />
      <KpiTile
        label="Open rate"
        value={formatPercent(t?.open_rate)}
        change={rateChange(t?.open_rate ?? 0, p?.open_rate ?? 0, (p?.emails_delivered ?? 0) > 0)}
        compareLabel={vs}
        sub={`${formatNumber(t?.opens)} unique opens`}
        hint="Share of delivered emails opened at least once. Apple Mail opens some emails automatically, so treat this as a rough guide and lean on clicks."
        trend={rateTrendOf(s, (d) => d.opens, (d) => d.emails_sent)}
        loading={loading}
      />
      <KpiTile
        label="Click rate"
        value={formatPercent(t?.click_rate)}
        change={rateChange(t?.click_rate ?? 0, p?.click_rate ?? 0, (p?.emails_delivered ?? 0) + (p?.sms_sent ?? 0) > 0)}
        compareLabel={vs}
        sub={`${formatNumber(t?.clicks)} unique clicks`}
        hint="Share of delivered emails and texts with at least one link clicked."
        trend={rateTrendOf(s, (d) => d.clicks, (d) => d.emails_sent + d.sms_sent)}
        loading={loading}
      />
      <KpiTile
        label="New profiles"
        value={formatNumber(t?.new_profiles)}
        change={change(t?.new_profiles ?? 0, p?.new_profiles ?? 0, "count")}
        compareLabel={vs}
        sub={`${formatNumber(t?.profiles)} in total`}
        trend={trendOf(s, (d) => d.new_profiles)}
        loading={loading}
      />
    </div>
  );
}

function Funnel({ data, loading }: { data: AnalyticsOverview | undefined; loading: boolean }) {
  const t = data?.totals;
  const steps = [
    { label: "Delivered", value: t?.emails_delivered ?? 0 },
    { label: "Opened", value: t?.opens ?? 0 },
    { label: "Clicked", value: t?.email_clicks ?? 0 },
    { label: "Ordered", value: t?.attributed_orders ?? 0 },
  ];
  // Orders are credited on an open or a click, so the last step compares with opens.
  const base = (i: number) => (i === 3 ? steps[1] : steps[i - 1]);
  const top = steps[0].value;
  return (
    <Card
      title={
        <span className="flex items-center gap-1">
          Email to order
          <Hint text="Each step counts people once per email. Orders include any order credited to an email or text in this range." />
        </span>
      }
    >
      {loading ? (
        <div className="space-y-3">
          {steps.map((s) => (
            <Skeleton key={s.label} className="h-9" />
          ))}
        </div>
      ) : top === 0 ? (
        <p className="py-10 text-center text-sm text-zinc-500">No emails delivered in this range.</p>
      ) : (
        <ol className="space-y-3">
          {steps.map((s, i) => {
            const prev = i > 0 ? base(i).value : 0;
            return (
              <li key={s.label}>
                <div className="mb-1 flex items-baseline justify-between gap-2 text-sm">
                  <span className="text-zinc-700">{s.label}</span>
                  <span className="flex items-baseline gap-2">
                    {i > 0 ? <span className="text-xs text-zinc-500">{prev ? formatPercent(s.value / prev, 1) : "—"} of {base(i).label.toLowerCase()}</span> : null}
                    <span className="font-medium tabular-nums text-zinc-900">{formatNumber(s.value)}</span>
                  </span>
                </div>
                <div className="h-2 rounded-full bg-zinc-100">
                  <div className="h-full rounded-full bg-accent-500" style={{ width: `${Math.max(s.value ? 1 : 0, (s.value / top) * 100)}%` }} />
                </div>
              </li>
            );
          })}
        </ol>
      )}
    </Card>
  );
}

type Level = "good" | "watch" | "bad" | "none";

const levelStyle: Record<Level, { text: string; bar: string; icon: ReactNode; label: string }> = {
  good: { text: "text-accent-800", bar: "bg-accent-500", icon: <CheckCircle2 className="size-3.5" />, label: "Healthy" },
  watch: { text: "text-amber-700", bar: "bg-amber-500", icon: <AlertTriangle className="size-3.5" />, label: "Watch" },
  bad: { text: "text-red-700", bar: "bg-red-600", icon: <OctagonAlert className="size-3.5" />, label: "Too high" },
  none: { text: "text-zinc-500", bar: "bg-zinc-300", icon: null, label: "No sends" },
};

function Health({ data, loading, vs }: { data: AnalyticsOverview | undefined; loading: boolean; vs: string }) {
  const t = data?.totals;
  const p = data?.previous;
  const sent = (t?.emails_sent ?? 0) > 0;
  const prevSent = (p?.emails_sent ?? 0) > 0;
  // Bounce and complaint limits match the automatic pause (src/deliverability.rs); unsubscribes have no limit.
  const rows = [
    {
      label: "Bounce rate",
      value: t?.bounce_rate ?? 0,
      prev: p?.bounce_rate ?? 0,
      watch: 0.02,
      limit: 0.04,
      digits: 2,
      note: "Email pauses above 4% over 7 days. Remove old or bought addresses to bring it down.",
    },
    {
      label: "Spam complaints",
      value: t?.complaint_rate ?? 0,
      prev: p?.complaint_rate ?? 0,
      watch: 0.0004,
      limit: 0.0008,
      digits: 3,
      note: "Email pauses above 0.08% over 7 days. Only email people who asked to hear from you.",
    },
    {
      label: "Unsubscribe rate",
      value: t?.unsubscribe_rate ?? 0,
      prev: p?.unsubscribe_rate ?? 0,
      watch: 0.005,
      limit: 0.01,
      digits: 2,
      note: "Under 0.5% is normal. Higher usually means sending too often or to the wrong people.",
    },
  ];
  return (
    <Card title="List health">
      {loading ? (
        <div className="space-y-4">
          {rows.map((r) => (
            <Skeleton key={r.label} className="h-10" />
          ))}
        </div>
      ) : (
        <ul className="space-y-4">
          {rows.map((r) => {
            const level: Level = !sent ? "none" : r.value >= r.limit ? "bad" : r.value >= r.watch ? "watch" : "good";
            const st = levelStyle[level];
            const scale = r.limit * 1.25;
            return (
              <li key={r.label}>
                <div className="mb-1 flex items-center justify-between gap-2 text-sm">
                  <span className="flex items-center gap-1 text-zinc-700">
                    {r.label}
                    <Hint text={r.note} />
                  </span>
                  <span className="flex items-center gap-2">
                    <span className={cx("inline-flex items-center gap-1 text-xs font-medium", st.text)}>
                      {st.icon}
                      {st.label}
                    </span>
                    <DeltaBadge value={sent && prevSent ? change(r.value, r.prev, "rate", r.digits) : null} upIsGood={false} label={vs} />
                    <span className="w-14 text-right font-medium tabular-nums text-zinc-900">{formatPercent(r.value, r.digits)}</span>
                  </span>
                </div>
                <div className="relative h-2 rounded-full bg-zinc-100">
                  <div className={cx("h-full rounded-full", st.bar)} style={{ width: `${Math.min(100, (r.value / scale) * 100)}%` }} />
                  <div className="absolute -top-0.5 h-3 w-px bg-zinc-400" style={{ left: `${(r.limit / scale) * 100}%` }} title={`Limit ${formatPercent(r.limit, r.digits)}`} />
                </div>
              </li>
            );
          })}
        </ul>
      )}
    </Card>
  );
}

type ActivityView = "sent" | "engagement" | "profiles";

function Activity({ data, loading }: { data: AnalyticsOverview | undefined; loading: boolean }) {
  const [view, setView] = useState<ActivityView>("sent");
  const series = data?.series ?? [];
  const legend =
    view === "sent" ? (
      <>
        <LegendDot color={color.email}>Email</LegendDot>
        <LegendDot color={color.sms}>SMS</LegendDot>
      </>
    ) : view === "engagement" ? (
      <>
        <LegendDot color="#219464">Opens</LegendDot>
        <LegendDot color="#6d5bd0">Clicks</LegendDot>
      </>
    ) : null;
  return (
    <Card
      title="Activity"
      actions={
        <Segmented
          size="sm"
          value={view}
          onChange={setView}
          options={[
            { value: "sent", label: "Sent" },
            { value: "engagement", label: "Opens & clicks" },
            { value: "profiles", label: "New profiles" },
          ]}
        />
      }
    >
      {legend ? <div className="mb-2 flex justify-end gap-3">{legend}</div> : null}
      {view === "sent" ? (
        <BarSeriesChart
          loading={loading}
          data={series}
          xKey="date"
          showTotal
          format={(n) => formatCompact(n)}
          series={[
            { key: "emails_sent", label: "Email", color: color.email },
            { key: "sms_sent", label: "SMS", color: color.sms },
          ]}
        />
      ) : view === "engagement" ? (
        <TimeSeriesChart
          loading={loading}
          data={series}
          xKey="date"
          format={(n) => formatCompact(n)}
          hideLegend
          series={[
            { key: "opens", label: "Opens" },
            { key: "clicks", label: "Clicks" },
          ]}
        />
      ) : (
        <BarSeriesChart loading={loading} data={series} xKey="date" format={(n) => formatCompact(n)} series={[{ key: "new_profiles", label: "New profiles", color: "#219464" }]} />
      )}
    </Card>
  );
}

/** Revenue with a bar scaled to the table's top row, so the leader is visible at a glance. */
function RevenueCell({ revenue, max }: { revenue: number; max: number }) {
  return (
    <div className="flex items-center justify-end gap-3">
      <div className="hidden h-1.5 w-24 rounded-full bg-zinc-100 sm:block">
        <div className="h-full rounded-full bg-accent-500" style={{ width: `${max ? (revenue / max) * 100 : 0}%` }} />
      </div>
      <span className="w-20 font-medium tabular-nums text-zinc-900">{formatCurrency(revenue)}</span>
    </div>
  );
}

function SectionHeader({ title, to, cta }: { title: string; to: string; cta: string }) {
  return (
    <div className="mb-2 flex items-center justify-between">
      <h2 className="text-sm font-semibold text-zinc-900">{title}</h2>
      <Link to={to} className="inline-flex items-center gap-1 text-xs font-medium text-zinc-500 hover:text-zinc-900">
        {cta} <ArrowRight className="size-3" />
      </Link>
    </div>
  );
}

function CampaignsTable({ data, loading }: { data: AnalyticsOverview | undefined; loading: boolean }) {
  const { account } = useSession();
  const rows = data?.top_campaigns ?? [];
  const max = Math.max(0, ...rows.map((r) => r.revenue));
  return (
    <div>
      <SectionHeader title="Campaigns sent in this range" to="/campaigns" cta="All campaigns" />
      <Table>
        <thead>
          <tr>
            <Th>Campaign</Th>
            <Th className="text-right">Recipients</Th>
            <Th className="text-right">Open</Th>
            <Th className="text-right">Click</Th>
            <Th className="text-right">Orders</Th>
            <Th className="text-right">Revenue</Th>
          </tr>
        </thead>
        <tbody>
          {loading ? (
            <SkeletonRows cols={6} rows={4} />
          ) : rows.length === 0 ? (
            <EmptyRow cols={6}>
              <div className="px-4 py-8 text-center">
                <p className="text-sm text-zinc-600">No campaigns sent in this range.</p>
                <Link to="/campaigns" className="mt-1 inline-flex items-center gap-1 text-xs font-medium text-accent-700 hover:text-accent-800">
                  Create a campaign <ArrowRight className="size-3" />
                </Link>
              </div>
            </EmptyRow>
          ) : (
            rows.map((c) => (
              <tr key={c.id} className="hover:bg-zinc-50">
                <Td>
                  <Link to={`/campaigns/${c.id}`} className="font-medium text-zinc-900 hover:text-accent-700">
                    {c.name}
                  </Link>
                  <div className="text-xs text-zinc-500">{formatDate(c.sent_at, account.timezone)}</div>
                </Td>
                <Td className="text-right tabular-nums">{formatNumber(c.recipients)}</Td>
                <Td className="text-right tabular-nums">{formatPercent(c.open_rate)}</Td>
                <Td className="text-right tabular-nums">{formatPercent(c.click_rate)}</Td>
                <Td className="text-right tabular-nums">{formatNumber(c.orders)}</Td>
                <Td className="text-right">
                  <RevenueCell revenue={c.revenue} max={max} />
                </Td>
              </tr>
            ))
          )}
        </tbody>
      </Table>
    </div>
  );
}

function FlowsTable({ data, loading }: { data: AnalyticsOverview | undefined; loading: boolean }) {
  const rows = [...(data?.top_flows ?? [])].sort((a, b) => b.revenue - a.revenue || b.recipients - a.recipients);
  const max = Math.max(0, ...rows.map((r) => r.revenue));
  return (
    <div>
      <SectionHeader title="Flows in this range" to="/flows" cta="All flows" />
      <Table>
        <thead>
          <tr>
            <Th>Flow</Th>
            <Th className="text-right">Sent</Th>
            <Th className="text-right">Open</Th>
            <Th className="text-right">Click</Th>
            <Th className="text-right">Orders</Th>
            <Th className="text-right">Revenue</Th>
          </tr>
        </thead>
        <tbody>
          {loading ? (
            <SkeletonRows cols={6} rows={4} />
          ) : rows.length === 0 ? (
            <EmptyRow cols={6}>
              <div className="px-4 py-8 text-center">
                <p className="text-sm text-zinc-600">No live flows yet.</p>
                <Link to="/flows?create=1" className="mt-1 inline-flex items-center gap-1 text-xs font-medium text-accent-700 hover:text-accent-800">
                  Turn on a welcome series <ArrowRight className="size-3" />
                </Link>
              </div>
            </EmptyRow>
          ) : (
            rows.map((f) => (
              <tr key={f.id} className="hover:bg-zinc-50">
                <Td>
                  <span className="flex items-center gap-2">
                    <Link to={`/flows/${f.id}`} className="font-medium text-zinc-900 hover:text-accent-700">
                      {f.name}
                    </Link>
                    <StatusBadge status={f.status} />
                  </span>
                </Td>
                <Td className="text-right tabular-nums">{formatNumber(f.recipients)}</Td>
                <Td className="text-right tabular-nums">{f.recipients ? formatPercent(f.open_rate) : "—"}</Td>
                <Td className="text-right tabular-nums">{f.recipients ? formatPercent(f.click_rate) : "—"}</Td>
                <Td className="text-right tabular-nums">{formatNumber(f.orders)}</Td>
                <Td className="text-right">
                  <RevenueCell revenue={f.revenue} max={max} />
                </Td>
              </tr>
            ))
          )}
        </tbody>
      </Table>
    </div>
  );
}

function GetStarted({ days }: { days: number }) {
  const steps = [
    { icon: <Workflow className="size-4" />, title: "Turn on a flow", body: "A welcome series greets every new subscriber and runs on its own.", to: "/flows?create=1", cta: "Pick a flow" },
    { icon: <Mail className="size-4" />, title: "Send a campaign", body: "Write one email or text and send it to a list or segment.", to: "/campaigns", cta: "Create a campaign" },
    { icon: <FileText className="size-4" />, title: "Grow your list", body: "Add a sign-up form to your site so there's someone to send to.", to: "/forms", cta: "Make a form" },
  ];
  return (
    <Card padded={false}>
      <div className="flex flex-col items-center px-6 pt-10 pb-6 text-center">
        <div className="mb-3 rounded-full bg-accent-50 p-3 text-accent-700">
          <Info className="size-5" />
        </div>
        <h2 className="text-base font-semibold text-zinc-900">Nothing to report yet</h2>
        <p className="mt-1 max-w-md text-sm text-zinc-500">
          No messages, sign-ups or orders in the last {days} days. Once you send, this page shows the revenue your emails and texts bring in, how people engage and how
          healthy your list is.
        </p>
      </div>
      <div className="grid grid-cols-1 gap-3 border-t border-zinc-100 p-4 sm:grid-cols-3">
        {steps.map((s) => (
          <Link key={s.title} to={s.to} className="group rounded-lg border border-zinc-200 p-4 transition-colors hover:border-accent-300 hover:bg-accent-50/40">
            <div className="mb-2 inline-flex rounded-md bg-zinc-100 p-1.5 text-zinc-600 group-hover:bg-accent-100 group-hover:text-accent-800">{s.icon}</div>
            <div className="text-sm font-medium text-zinc-900">{s.title}</div>
            <p className="mt-0.5 text-xs text-zinc-500">{s.body}</p>
            <span className="mt-3 inline-flex items-center gap-1 text-xs font-medium text-accent-700">
              {s.cta} <ArrowRight className="size-3" />
            </span>
          </Link>
        ))}
      </div>
    </Card>
  );
}
