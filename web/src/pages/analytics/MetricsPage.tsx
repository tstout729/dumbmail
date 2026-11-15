import { useState } from "react";
import { useSearchParams } from "react-router";
import { Gauge, Search } from "lucide-react";
import type { Interval, Measurement, MetricQuery, MetricQueryResult } from "../../api";
import { useMetricQuery, useMetrics, useSession } from "../../api";
import { TimeSeriesChart, type Series } from "../../components/charts/TimeSeriesChart";
import { Card, EmptyState, Field, Input, Page, PageHeader, Select, Skeleton, cx } from "../../components/ui";
import { formatCurrency, formatNumber, formatRelative } from "../../lib/format";
import { useDebouncedValue } from "../../lib/hooks";
import { utcToZoned, zonedToUtc } from "../../lib/tz";

const ranges = [
  { value: "7", label: "Last 7 days" },
  { value: "30", label: "Last 30 days" },
  { value: "90", label: "Last 90 days" },
  { value: "365", label: "Last 12 months" },
];

const maxGroups = 5;

/** Pivot grouped rows into one row per date; keep the biggest groups and fold the rest into "Other". */
function pivot(result: MetricQueryResult | undefined): { rows: Record<string, unknown>[]; series: Series[] } {
  if (!result) return { rows: [], series: [] };
  const grouped = result.series.some((r) => r.group !== undefined && r.group !== null);
  if (!grouped) return { rows: result.series, series: [{ key: "value", label: "Value" }] };
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
  const series: Series[] = ranked.slice(0, maxGroups).map((g, i) => ({ key: `g_${i}`, label: g }));
  if (hasOther) series.push({ key: "g_other", label: "Other", color: "#a1a1aa" });
  const rows = [...byDate.values()].sort((a, b) => String(a.date).localeCompare(String(b.date)));
  for (const row of rows) for (const s of series) row[s.key] ??= 0;
  return { rows, series };
}

export function MetricsPage() {
  const { account } = useSession();
  const metrics = useMetrics();
  const [params, setParams] = useSearchParams();
  const [filter, setFilter] = useState("");
  const [measurement, setMeasurement] = useState<Measurement>("count");
  const [interval, setIntervalUnit] = useState<Interval>("day");
  const [range, setRange] = useState("30");
  const [groupBy, setGroupBy] = useState("");
  const debouncedGroupBy = useDebouncedValue(groupBy.trim(), 500);

  const all = metrics.data ?? [];
  const selected = params.get("metric") ?? all[0]?.name ?? "";
  // Fixed when the page opens so the query key (and its date range) stays stable across renders.
  const [now] = useState(() => Date.now());
  const tz = account.timezone;
  const today = utcToZoned(new Date(now).toISOString(), tz).date;
  const startDate = utcToZoned(new Date(now - Number(range) * 864e5).toISOString(), tz).date;

  const query: MetricQuery | null = selected
    ? {
        metric: selected,
        measurement,
        interval,
        start: zonedToUtc(startDate, "00:00", tz),
        end: zonedToUtc(today, "23:59", tz),
        group_by: debouncedGroupBy || undefined,
      }
    : null;
  const result = useMetricQuery(query);
  const { rows, series } = pivot(result.data);
  const fmt = measurement === "sum_value" ? (n: number) => formatCurrency(n) : (n: number) => formatNumber(n);

  return (
    <Page wide>
      <PageHeader title="Metrics" description="Every event you track, and how it trends." />
      <div className="grid grid-cols-1 gap-4 lg:grid-cols-[300px_minmax(0,1fr)]">
        <Card padded={false}>
          <div className="border-b border-zinc-100 p-2">
            <div className="relative">
              <Search className="pointer-events-none absolute left-2.5 top-2.5 size-3.5 text-zinc-400" />
              <Input value={filter} onChange={(e) => setFilter(e.target.value)} placeholder="Filter metrics" className="pl-8" />
            </div>
          </div>
          {metrics.isPending ? (
            <div className="space-y-2 p-3">
              {[0, 1, 2, 3, 4].map((i) => (
                <Skeleton key={i} className="h-9" />
              ))}
            </div>
          ) : all.length === 0 ? (
            <EmptyState icon={<Gauge className="size-5" />} title="No metrics yet" description="Send events from your site or the API and they show up here." />
          ) : (
            <ul className="max-h-[70vh] overflow-y-auto p-1">
              {all
                .filter((m) => m.name.toLowerCase().includes(filter.toLowerCase()))
                .map((m) => (
                  <li key={m.id}>
                    <button
                      type="button"
                      onClick={() => setParams({ metric: m.name }, { replace: true })}
                      className={cx(
                        "flex w-full items-center justify-between gap-2 rounded-md px-3 py-2 text-left",
                        m.name === selected ? "bg-accent-50 text-accent-900" : "hover:bg-zinc-50",
                      )}
                    >
                      <span className="min-w-0">
                        <span className="block truncate text-sm font-medium">{m.name}</span>
                        <span className="block text-[11px] text-zinc-500">Last seen {formatRelative(m.last_seen)}</span>
                      </span>
                      <span className="text-xs tabular-nums text-zinc-500">{formatNumber(m.count_30d)}</span>
                    </button>
                  </li>
                ))}
            </ul>
          )}
        </Card>
        <div className="space-y-4">
          <Card>
            <div className="grid grid-cols-1 gap-3 sm:grid-cols-2 xl:grid-cols-4">
              <Field label="Measure">
                <Select value={measurement} onChange={(e) => setMeasurement(e.target.value as Measurement)}>
                  <option value="count">Total events</option>
                  <option value="unique_profiles">Unique people</option>
                  <option value="sum_value">Sum of value</option>
                </Select>
              </Field>
              <Field label="Interval">
                <Select value={interval} onChange={(e) => setIntervalUnit(e.target.value as Interval)}>
                  <option value="day">Daily</option>
                  <option value="week">Weekly</option>
                  <option value="month">Monthly</option>
                </Select>
              </Field>
              <Field label="Range">
                <Select value={range} onChange={(e) => setRange(e.target.value)}>
                  {ranges.map((r) => (
                    <option key={r.value} value={r.value}>
                      {r.label}
                    </option>
                  ))}
                </Select>
              </Field>
              <Field label="Group by (optional)">
                <Input value={groupBy} onChange={(e) => setGroupBy(e.target.value)} placeholder="event property, e.g. source" />
              </Field>
            </div>
          </Card>
          <Card
            title={selected || "Pick a metric"}
            actions={result.data ? <span className="text-sm font-semibold tabular-nums">{fmt(result.data.total)} total</span> : null}
          >
            {!selected ? (
              <EmptyState title="Pick a metric on the left" />
            ) : result.error ? (
              <p className="py-10 text-center text-sm text-red-600">{result.error.message}</p>
            ) : (
              <TimeSeriesChart loading={result.isPending} data={rows} xKey="date" series={series} format={fmt} height={320} />
            )}
          </Card>
        </div>
      </div>
    </Page>
  );
}
