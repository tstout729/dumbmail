import { useState, type ReactNode } from "react";
import { Link, useSearchParams } from "react-router";
import { ArrowRight, Code2, Gauge, Globe, Search } from "lucide-react";
import type { EventRow, Interval, Measurement, MetricQuery, MetricQueryResult, MetricSummary } from "../../api";
import { useEvents, useMetricQuery, useMetrics, useSession } from "../../api";
import { DeltaBadge, LegendDot, Sparkline, change } from "../../components/analytics/kit";
import { BarSeriesChart } from "../../components/charts/BarSeriesChart";
import { seriesColors, type Series } from "../../components/charts/TimeSeriesChart";
import { Card, EmptyRow, Input, Page, PageHeader, Segmented, Skeleton, SkeletonRows, Table, Td, Th, cx } from "../../components/ui";
import { formatCompact, formatCurrency, formatDateTime, formatNumber, formatPercent, formatRelative, formatShortDate } from "../../lib/format";
import { utcToZoned, zonedToUtc } from "../../lib/tz";

type RangeKey = "7" | "30" | "90" | "365";

const ranges: { value: RangeKey; label: string }[] = [
  { value: "7", label: "7D" },
  { value: "30", label: "30D" },
  { value: "90", label: "90D" },
  { value: "365", label: "12M" },
];

const maxGroups = 5;
const otherColor = "#a1a1aa";

/** What each built-in metric means, and where it comes from. Anything else is a custom metric. */
const known: Record<string, { group: "Store & website" | "Messages" | "Audience"; about: string }> = {
  "Placed Order": { group: "Store & website", about: "Someone completed a purchase. The value is the order total." },
  "Started Checkout": { group: "Store & website", about: "Someone reached checkout. The value is the cart total." },
  "Added to Cart": { group: "Store & website", about: "Someone added an item to their cart." },
  "Viewed Product": { group: "Store & website", about: "Someone looked at a product page." },
  "Active on Site": { group: "Store & website", about: "A known visitor came to your site (once per visit)." },
  "Received Email": { group: "Messages", about: "DumbMail delivered one of your emails." },
  "Opened Email": { group: "Messages", about: "Someone opened one of your emails." },
  "Clicked Email": { group: "Messages", about: "Someone clicked a link in one of your emails." },
  "Bounced Email": { group: "Messages", about: "An email couldn't be delivered." },
  "Marked Email as Spam": { group: "Messages", about: "Someone reported one of your emails as spam." },
  "Received SMS": { group: "Messages", about: "DumbMail delivered one of your texts." },
  "Clicked SMS": { group: "Messages", about: "Someone clicked a link in one of your texts." },
  "Received SMS Reply": { group: "Messages", about: "Someone replied to one of your texts." },
  "Subscribed to List": { group: "Audience", about: "Someone joined a list, from a form, an import or the API." },
  "Subscribed to SMS": { group: "Audience", about: "Someone agreed to get texts." },
  Unsubscribed: { group: "Audience", about: "Someone unsubscribed from email." },
  "Unsubscribed from SMS": { group: "Audience", about: "Someone stopped texts (replied STOP or similar)." },
};
const groupOrder = ["Store & website", "Messages", "Audience", "Custom"] as const;

function groupOf(name: string) {
  return known[name]?.group ?? "Custom";
}

/** Pivot grouped rows into one row per date; keep the biggest groups and fold the rest into "Other". */
function pivot(result: MetricQueryResult | undefined): { rows: Record<string, unknown>[]; series: Series[]; breakdown: { label: string; value: number; color: string }[] } {
  if (!result) return { rows: [], series: [], breakdown: [] };
  const grouped = result.series.some((r) => r.group !== undefined && r.group !== null);
  if (!grouped) return { rows: result.series, series: [{ key: "value", label: "Value", color: seriesColors[0] }], breakdown: [] };
  const totals = new Map<string, number>();
  for (const r of result.series) totals.set(r.group ?? "(none)", (totals.get(r.group ?? "(none)") ?? 0) + r.value);
  const ranked = [...totals.entries()].sort((a, b) => b[1] - a[1]).map(([g]) => g);
  const keep = new Set(ranked.slice(0, maxGroups));
  const hasOther = ranked.length > maxGroups;
  const byDate = new Map<string, Record<string, unknown>>();
  for (const r of result.series) {
    const g = r.group ?? "(none)";
    const key = keep.has(g) ? `g_${ranked.indexOf(g)}` : "g_other";
    const row = byDate.get(r.date) ?? { date: r.date };
    row[key] = ((row[key] as number | undefined) ?? 0) + r.value;
    byDate.set(r.date, row);
  }
  const series: Series[] = ranked.slice(0, maxGroups).map((g, i) => ({ key: `g_${i}`, label: g, color: seriesColors[i] }));
  if (hasOther) series.push({ key: "g_other", label: "Other", color: otherColor });
  const rows = [...byDate.values()].sort((a, b) => String(a.date).localeCompare(String(b.date)));
  for (const row of rows) for (const s of series) row[s.key] ??= 0;
  const breakdown = ranked.slice(0, maxGroups).map((g, i) => ({ label: g, value: totals.get(g) ?? 0, color: seriesColors[i] }));
  if (hasOther) breakdown.push({ label: `Other (${ranked.length - maxGroups})`, value: ranked.slice(maxGroups).reduce((s, g) => s + (totals.get(g) ?? 0), 0), color: otherColor });
  return { rows, series, breakdown };
}

/** Fill missing buckets with zero so quiet days show as gaps, not as a line skipping over them. */
function fillDays(rows: Record<string, unknown>[], start: string, end: string, interval: Interval, keys: string[]): Record<string, unknown>[] {
  if (interval !== "day") return rows;
  const byDate = new Map(rows.map((r) => [String(r.date), r]));
  const out: Record<string, unknown>[] = [];
  const d = new Date(`${start}T00:00:00Z`);
  const last = new Date(`${end}T00:00:00Z`);
  while (d <= last) {
    const key = d.toISOString().slice(0, 10);
    out.push(byDate.get(key) ?? Object.fromEntries([["date", key], ...keys.map((k) => [k, 0])]));
    d.setUTCDate(d.getUTCDate() + 1);
  }
  return out;
}

/** Property names seen on recent events, for the "Break down by" menu. */
function propertyKeys(events: EventRow[] | undefined): string[] {
  const keys = new Set<string>();
  for (const e of events ?? [])
    for (const [k, v] of Object.entries(e.properties ?? {})) if (v !== null && ["string", "number", "boolean"].includes(typeof v)) keys.add(k);
  return [...keys].sort();
}

export function MetricsPage() {
  const metrics = useMetrics();
  const [params, setParams] = useSearchParams();
  const [filter, setFilter] = useState("");
  const all = metrics.data ?? [];
  const byPopularity = [...all].sort((a, b) => b.count_30d - a.count_30d);
  const fallback = all.find((m) => m.name === "Placed Order") ?? byPopularity[0];
  const selected = all.find((m) => m.name === params.get("metric")) ?? fallback;

  return (
    <Page wide>
      <PageHeader title="Metrics" description="Everything DumbMail records from your store, your website and your messages, and how it trends." />
      {metrics.error ? <p className="mb-4 text-sm text-red-600">{metrics.error.message}</p> : null}
      {!metrics.isPending && all.length === 0 ? (
        <NoMetrics />
      ) : (
        <div className="grid grid-cols-1 gap-4 lg:grid-cols-[280px_minmax(0,1fr)]">
          <MetricList
            metrics={all}
            loading={metrics.isPending}
            filter={filter}
            onFilter={setFilter}
            selected={selected?.name}
            onSelect={(name) => setParams({ metric: name }, { replace: true })}
          />
          {selected ? <MetricDetail key={selected.name} metric={selected} /> : <Skeleton className="h-96" />}
        </div>
      )}
    </Page>
  );
}

type ListProps = {
  metrics: MetricSummary[];
  loading: boolean;
  filter: string;
  onFilter: (v: string) => void;
  selected: string | undefined;
  onSelect: (name: string) => void;
};

function MetricList({ metrics, loading, filter, onFilter, selected, onSelect }: ListProps) {
  const q = filter.trim().toLowerCase();
  const shown = metrics.filter((m) => m.name.toLowerCase().includes(q));
  return (
    <Card padded={false} className="self-start lg:sticky lg:top-4">
      <div className="border-b border-zinc-100 p-2">
        <div className="relative">
          <Search className="pointer-events-none absolute top-2.5 left-2.5 size-3.5 text-zinc-400" />
          <Input value={filter} onChange={(e) => onFilter(e.target.value)} placeholder="Find a metric" className="pl-8" aria-label="Find a metric" />
        </div>
      </div>
      {loading ? (
        <div className="space-y-2 p-3">
          {[0, 1, 2, 3, 4].map((i) => (
            <Skeleton key={i} className="h-11" />
          ))}
        </div>
      ) : shown.length === 0 ? (
        <p className="px-4 py-8 text-center text-sm text-zinc-500">No metric matches “{filter}”.</p>
      ) : (
        <div className="max-h-[calc(100vh-12rem)] overflow-y-auto p-1.5">
          {groupOrder.map((group) => {
            const items = shown.filter((m) => groupOf(m.name) === group).sort((a, b) => b.count_30d - a.count_30d || a.name.localeCompare(b.name));
            if (items.length === 0) return null;
            return (
              <div key={group} className="mb-1">
                <div className="px-2.5 pt-2 pb-1 text-[11px] font-semibold tracking-wide text-zinc-400 uppercase">{group}</div>
                <ul>
                  {items.map((m) => {
                    const active = m.name === selected;
                    return (
                      <li key={m.id}>
                        <button
                          type="button"
                          onClick={() => onSelect(m.name)}
                          aria-current={active ? "true" : undefined}
                          className={cx(
                            "grid w-full grid-cols-[minmax(0,1fr)_56px] items-center gap-3 rounded-md px-2.5 py-2 text-left",
                            active ? "bg-accent-50 ring-1 ring-accent-200 ring-inset" : "hover:bg-zinc-50",
                          )}
                        >
                          <span className="min-w-0">
                            <span className={cx("block truncate text-sm font-medium", active ? "text-accent-900" : "text-zinc-900")}>{m.name}</span>
                            <span className="block text-[11px] text-zinc-500">
                              {m.count_30d ? `${formatCompact(m.count_30d)} in 30 days` : `Last seen ${formatRelative(m.last_seen)}`}
                            </span>
                          </span>
                          <Sparkline values={m.trend} className="h-6" color={active ? "#147650" : "#a1a1aa"} />
                        </button>
                      </li>
                    );
                  })}
                </ul>
              </div>
            );
          })}
        </div>
      )}
    </Card>
  );
}

function MetricDetail({ metric }: { metric: MetricSummary }) {
  const { account } = useSession();
  const tz = account.timezone;
  const [measurement, setMeasurement] = useState<Measurement>("count");
  const [range, setRange] = useState<RangeKey>("30");
  const [interval, setIntervalUnit] = useState<Interval>("day");
  const [groupBy, setGroupBy] = useState("");
  // Fixed when the metric opens so the query keys (and their date ranges) stay stable across renders.
  const [now] = useState(() => Date.now());
  const recent = useEvents({ metric: metric.name, limit: 50 });

  const days = Number(range);
  const today = utcToZoned(new Date(now).toISOString(), tz).date;
  const startDate = utcToZoned(new Date(now - (days - 1) * 864e5).toISOString(), tz).date;
  const prevStart = utcToZoned(new Date(now - (2 * days - 1) * 864e5).toISOString(), tz).date;
  const prevEnd = utcToZoned(new Date(now - days * 864e5).toISOString(), tz).date;
  const base = { metric: metric.name, measurement, interval };
  const query: MetricQuery = { ...base, start: zonedToUtc(startDate, "00:00", tz), end: zonedToUtc(today, "23:59", tz), group_by: groupBy || undefined };
  const prevQuery: MetricQuery = { ...base, start: zonedToUtc(prevStart, "00:00", tz), end: zonedToUtc(prevEnd, "23:59", tz) };
  const result = useMetricQuery(query);
  const previous = useMetricQuery(prevQuery);

  const { rows: raw, series, breakdown } = pivot(result.data);
  const rows = fillDays(raw, startDate, today, interval, series.map((s) => s.key));
  const isMoney = measurement === "sum_value";
  const fmt = isMoney ? (n: number) => formatCurrency(n) : (n: number) => formatNumber(n);
  const fmtAxis = isMoney ? (n: number) => (n >= 1000 ? `$${formatCompact(n)}` : formatCurrency(n)) : (n: number) => formatCompact(n);
  const total = result.data?.total ?? 0;
  const perBucket = rows.length ? total / rows.length : 0;
  const peak = rows.reduce<{ date: string; value: number } | null>((best, r) => {
    const v = series.reduce((s, x) => s + Number(r[x.key] ?? 0), 0);
    return !best || v > best.value ? { date: String(r.date), value: v } : best;
  }, null);
  const unit = interval === "day" ? "day" : interval === "week" ? "week" : "month";
  const keys = propertyKeys(recent.data?.data);
  const vs = `vs previous ${days === 365 ? "12 months" : `${days} days`}`;

  function pickRange(v: RangeKey) {
    setRange(v);
    // Keep the chart readable: daily bars over a year are too thin, monthly over a week is one bar.
    if (v === "365" && interval === "day") setIntervalUnit("week");
    if (v === "7" && interval !== "day") setIntervalUnit("day");
  }

  return (
    <div className="min-w-0 space-y-4">
      <Card padded={false}>
        <div className="flex flex-wrap items-start justify-between gap-3 border-b border-zinc-100 px-5 pt-4 pb-3">
          <div className="min-w-0">
            <div className="flex items-center gap-2">
              <h2 className="truncate text-base font-semibold text-zinc-950">{metric.name}</h2>
              <span className="rounded-full bg-zinc-100 px-2 py-0.5 text-[11px] font-medium text-zinc-600">{groupOf(metric.name)}</span>
            </div>
            <p className="mt-0.5 text-sm text-zinc-500">
              {known[metric.name]?.about ?? "A custom event sent from your site or the API."} Last seen {formatRelative(metric.last_seen)}.
            </p>
          </div>
          <Segmented value={range} onChange={pickRange} options={ranges} />
        </div>
        <div className="flex flex-wrap items-center gap-x-4 gap-y-2 px-5 py-3">
          <div className="flex items-center gap-2 text-xs text-zinc-500" role="group" aria-label="Measure">
            <span aria-hidden>Measure</span>
            <Segmented
              size="sm"
              value={measurement}
              onChange={setMeasurement}
              options={[
                { value: "count", label: "Events" },
                { value: "unique_profiles", label: "People" },
                ...(metric.has_value ? [{ value: "sum_value" as const, label: "Value" }] : []),
              ]}
            />
          </div>
          <div className="flex items-center gap-2 text-xs text-zinc-500" role="group" aria-label="Interval">
            <span aria-hidden>By</span>
            <Segmented
              size="sm"
              value={interval}
              onChange={setIntervalUnit}
              options={[
                { value: "day", label: "Day" },
                { value: "week", label: "Week" },
                { value: "month", label: "Month" },
              ]}
            />
          </div>
          <label className="flex items-center gap-2 text-xs text-zinc-500">
            Break down by
            <select
              value={groupBy}
              onChange={(e) => setGroupBy(e.target.value)}
              className="h-7 w-44 rounded-md border border-zinc-200 bg-white px-2 text-xs font-medium text-zinc-800 hover:border-zinc-300"
            >
              <option value="">Nothing</option>
              <option value="campaign">Campaign</option>
              <option value="flow">Flow</option>
              {keys.length ? (
                <optgroup label="Event properties">
                  {keys.map((k) => (
                    <option key={k} value={k}>
                      {k}
                    </option>
                  ))}
                </optgroup>
              ) : null}
            </select>
          </label>
        </div>
        <div className={cx("grid grid-cols-2 gap-px border-t border-zinc-100 bg-zinc-100 sm:grid-cols-4", result.isPlaceholderData && "opacity-60")}>
          <Summary label={measurement === "unique_profiles" ? "People" : isMoney ? "Total value" : "Total"} loading={result.isPending}>
            <span className="text-2xl font-semibold tracking-tight text-zinc-950">{fmt(total)}</span>
          </Summary>
          <Summary label={`Change ${vs}`} loading={result.isPending || previous.isPending}>
            {previous.data ? (
              <span className="flex items-center gap-2">
                <DeltaBadge value={change(total, previous.data.total, "count")} />
                <span className="text-xs text-zinc-500">from {fmt(previous.data.total)}</span>
              </span>
            ) : null}
          </Summary>
          <Summary label={`Average per ${unit}`} loading={result.isPending}>
            <span className="text-lg font-semibold text-zinc-900">{isMoney ? formatCurrency(perBucket) : perBucket >= 10 ? formatNumber(Math.round(perBucket)) : perBucket.toFixed(1)}</span>
          </Summary>
          <Summary label={`Busiest ${unit}`} loading={result.isPending}>
            {peak && peak.value > 0 ? (
              <span className="text-lg font-semibold text-zinc-900">
                {formatShortDate(peak.date)} <span className="text-xs font-normal text-zinc-500">{fmt(peak.value)}</span>
              </span>
            ) : (
              <span className="text-sm text-zinc-400">—</span>
            )}
          </Summary>
        </div>
        <div className="border-t border-zinc-100 p-5">
          {series.length > 1 ? (
            <div className="mb-3 flex flex-wrap justify-end gap-x-3 gap-y-1">
              {series.map((s) => (
                <LegendDot key={s.key} color={s.color ?? otherColor}>
                  {s.label}
                </LegendDot>
              ))}
            </div>
          ) : null}
          {result.error ? (
            <p className="py-16 text-center text-sm text-red-600">{result.error.message}</p>
          ) : !result.isPending && total === 0 ? (
            <div className="flex h-[280px] flex-col items-center justify-center text-center">
              <p className="text-sm font-medium text-zinc-700">No {metric.name} events in this range</p>
              <p className="mt-1 text-xs text-zinc-500">Last seen {formatRelative(metric.last_seen)}. Try a longer range.</p>
            </div>
          ) : (
            <BarSeriesChart loading={result.isPending} data={rows} xKey="date" series={series} format={fmtAxis} height={280} showTotal={series.length > 1} />
          )}
        </div>
      </Card>

      {breakdown.length > 0 ? <Breakdown title={groupBy} rows={breakdown} total={breakdown.reduce((s, b) => s + b.value, 0)} fmt={fmt} /> : null}

      <RecentEvents metric={metric.name} events={recent.data?.data} total={recent.data?.total} loading={recent.isPending} tz={tz} />
    </div>
  );
}

function Summary({ label, loading, children }: { label: string; loading: boolean; children: ReactNode }) {
  return (
    <div className="bg-white px-5 py-3">
      <div className="text-xs text-zinc-500">{label}</div>
      <div className="mt-1 flex min-h-8 items-center">{loading ? <Skeleton className="h-6 w-20" /> : children}</div>
    </div>
  );
}

function Breakdown({ title, rows, total, fmt }: { title: string; rows: { label: string; value: number; color: string }[]; total: number; fmt: (n: number) => string }) {
  const label = title === "campaign" ? "Campaign" : title === "flow" ? "Flow" : title;
  return (
    <Card title={`By ${label}`}>
      <ul className="space-y-3">
        {rows.map((r) => (
          <li key={r.label}>
            <div className="mb-1 flex items-center justify-between gap-3 text-sm">
              <LegendDot color={r.color}>
                <span className="truncate text-sm text-zinc-700">{r.label}</span>
              </LegendDot>
              <span className="flex items-baseline gap-2">
                <span className="text-xs text-zinc-500">{total ? formatPercent(r.value / total, 0) : "—"}</span>
                <span className="w-20 text-right font-medium tabular-nums text-zinc-900">{fmt(r.value)}</span>
              </span>
            </div>
            <div className="h-1.5 rounded-full bg-zinc-100">
              <div className="h-full rounded-full" style={{ width: `${total ? (r.value / total) * 100 : 0}%`, background: r.color }} />
            </div>
          </li>
        ))}
      </ul>
    </Card>
  );
}

/** The most telling properties first, so a row reads like a sentence. */
function describe(e: EventRow): string {
  const p = e.properties ?? {};
  const pick = ["item_names", "name", "product", "title", "order_id", "url", "source"];
  const parts: string[] = [];
  for (const k of pick) {
    const v = p[k];
    if (typeof v === "string" && v) parts.push(v);
    else if (Array.isArray(v) && v.length && v.every((x) => typeof x === "string")) parts.push(v.join(", "));
    if (parts.length === 2) break;
  }
  if (parts.length > 0) return parts.join(" · ");
  return Object.entries(p)
    .filter(([, v]) => ["string", "number", "boolean"].includes(typeof v))
    .slice(0, 2)
    .map(([k, v]) => `${k}: ${String(v)}`)
    .join(" · ");
}

function RecentEvents({ metric, events, total, loading, tz }: { metric: string; events: EventRow[] | undefined; total: number | undefined; loading: boolean; tz: string }) {
  const rows = (events ?? []).slice(0, 10);
  const hasValue = rows.some((e) => e.value !== null);
  return (
    <div>
      <div className="mb-2 flex items-baseline justify-between gap-2">
        <h2 className="text-sm font-semibold text-zinc-900">Latest activity</h2>
        {total ? <span className="text-xs text-zinc-500">{formatNumber(total)} {metric} events all time</span> : null}
      </div>
      <Table>
        <thead>
          <tr>
            <Th>Person</Th>
            <Th>Details</Th>
            {hasValue ? <Th className="text-right">Value</Th> : null}
            <Th className="text-right">When</Th>
          </tr>
        </thead>
        <tbody>
          {loading ? (
            <SkeletonRows cols={4} rows={5} />
          ) : rows.length === 0 ? (
            <EmptyRow cols={4}>
              <p className="px-4 py-6 text-center text-xs text-zinc-500">No events yet.</p>
            </EmptyRow>
          ) : (
            rows.map((e) => (
              <tr key={e.id} className="hover:bg-zinc-50">
                <Td className="max-w-56">
                  <Link to={`/audience/profiles/${e.profile_id}`} className="block truncate font-medium text-zinc-900 hover:text-accent-700">
                    {e.profile_label || "Unknown person"}
                  </Link>
                </Td>
                <Td className="max-w-80 truncate text-zinc-600">{describe(e) || <span className="text-zinc-400">—</span>}</Td>
                {hasValue ? <Td className="text-right tabular-nums">{e.value === null ? "—" : formatCurrency(e.value, true)}</Td> : null}
                <Td className="text-right whitespace-nowrap text-zinc-500" title={formatDateTime(e.occurred_at, tz)}>
                  {formatRelative(e.occurred_at)}
                </Td>
              </tr>
            ))
          )}
        </tbody>
      </Table>
    </div>
  );
}

function NoMetrics() {
  return (
    <Card padded={false}>
      <div className="flex flex-col items-center px-6 pt-10 pb-6 text-center">
        <div className="mb-3 rounded-full bg-accent-50 p-3 text-accent-700">
          <Gauge className="size-5" />
        </div>
        <h2 className="text-base font-semibold text-zinc-900">No events yet</h2>
        <p className="mt-1 max-w-lg text-sm text-zinc-500">
          Metrics are the things people do: view a product, start a checkout, place an order, open an email. They power your flows, segments and revenue numbers. Pick a
          way to start sending them.
        </p>
      </div>
      <div className="grid grid-cols-1 gap-3 border-t border-zinc-100 p-4 md:grid-cols-2">
        <Link to="/settings/website" className="group rounded-lg border border-zinc-200 p-4 transition-colors hover:border-accent-300 hover:bg-accent-50/40">
          <div className="mb-2 inline-flex rounded-md bg-zinc-100 p-1.5 text-zinc-600 group-hover:bg-accent-100 group-hover:text-accent-800">
            <Globe className="size-4" />
          </div>
          <div className="text-sm font-medium text-zinc-900">Add the website snippet</div>
          <p className="mt-0.5 text-xs text-zinc-500">
            One line of code. It tracks product views, carts, checkouts and orders on Shopify, WooCommerce and most other stores without extra setup.
          </p>
          <span className="mt-3 inline-flex items-center gap-1 text-xs font-medium text-accent-700">
            Get the snippet <ArrowRight className="size-3" />
          </span>
        </Link>
        <div className="rounded-lg border border-zinc-200 p-4">
          <div className="mb-2 inline-flex rounded-md bg-zinc-100 p-1.5 text-zinc-600">
            <Code2 className="size-4" />
          </div>
          <div className="text-sm font-medium text-zinc-900">Send events from your server</div>
          <p className="mt-0.5 text-xs text-zinc-500">
            Post any event with a <Link to="/settings/developers" className="font-medium text-accent-700 hover:text-accent-800">secret API key</Link>:
          </p>
          <pre className="mt-2 overflow-x-auto rounded-md bg-zinc-950 p-3 text-[11px] leading-relaxed text-zinc-100">
            {`curl ${window.location.origin}/api/events \\
  -H "Authorization: Bearer $DUMBMAIL_KEY" \\
  -H "Content-Type: application/json" \\
  -d '{"metric": "Placed Order", "value": 42.5,
       "profile": {"email": "sam@example.com"},
       "properties": {"order_id": "1001"}}'`}
          </pre>
        </div>
      </div>
    </Card>
  );
}
