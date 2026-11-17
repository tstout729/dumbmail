import { useState } from "react";
import { change, DeltaBadge } from "../../components/analytics/kit";
import { BarSeriesChart } from "../../components/charts/BarSeriesChart";
import { TimeSeriesChart } from "../../components/charts/TimeSeriesChart";
import { Card, EmptyRow, EmptyState, Select, SkeletonRows, Table, Td, Th, cx } from "../../components/ui";
import { BarChart3 } from "lucide-react";
import { formatBucket, formatValue, TIME_GROUPS } from "./format";
import type { MeasureKind, ReportColumn, ReportResult, ReportRow } from "./types";

const WEEKDAYS = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];

function rowLabel(groups: ReportColumn[], row: ReportRow, i: number): string {
  const g = groups[i]?.key ?? "";
  return TIME_GROUPS.includes(g) ? formatBucket(g, row.keys[i]) : (row.labels[i] ?? row.keys[i]);
}

function Totals({ result, measures }: { result: ReportResult; measures: ReportColumn[] }) {
  const vs = "vs the period before";
  return (
    <div className="grid grid-cols-2 gap-3 sm:grid-cols-3 lg:grid-cols-4">
      {measures.map((m) => {
        const cur = result.totals[m.key] ?? 0;
        const prev = result.previous?.[m.key];
        const upIsGood = !/unsubscrib|bounce|complaint|dropped/.test(m.key);
        return (
          <div key={m.key} className="rounded-lg border border-zinc-200 bg-white px-4 py-3 shadow-xs">
            <div className="text-xs font-medium text-zinc-500">{m.label}</div>
            <div className="mt-1 text-xl font-semibold tracking-tight tabular-nums text-zinc-950">{formatValue(m.kind as MeasureKind, cur)}</div>
            {prev !== undefined ? (
              <div className="mt-1 flex flex-wrap items-center gap-1.5 text-xs text-zinc-500">
                <DeltaBadge value={change(cur, prev, m.kind === "rate" ? "rate" : "count")} upIsGood={upIsGood} label={vs} />
                <span>from {formatValue(m.kind as MeasureKind, prev)}</span>
              </div>
            ) : null}
          </div>
        );
      })}
    </div>
  );
}

/** Day of week by hour, shaded by the chosen measure. */
function Heatmap({ rows, measure }: { rows: ReportRow[]; measure: ReportColumn }) {
  const cell = new Map(rows.map((r) => [`${r.keys[0]}-${r.keys[1]}`, r.values[measure.key] ?? 0]));
  const max = Math.max(0, ...cell.values());
  return (
    <div className="overflow-x-auto">
      <table className="min-w-[640px] border-separate border-spacing-0.5 text-[11px]">
        <thead>
          <tr>
            <th />
            {Array.from({ length: 24 }, (_, h) => (
              <th key={h} className="font-normal text-zinc-500">
                {h % 3 === 0 ? `${h}` : ""}
              </th>
            ))}
          </tr>
        </thead>
        <tbody>
          {WEEKDAYS.map((d, i) => (
            <tr key={d}>
              <th className="pr-2 text-right font-medium text-zinc-600">{d}</th>
              {Array.from({ length: 24 }, (_, h) => {
                const v = cell.get(`${i + 1}-${String(h).padStart(2, "0")}`) ?? 0;
                const t = max > 0 ? v / max : 0;
                return (
                  <td
                    key={h}
                    title={`${d} ${h}:00 · ${measure.label}: ${formatValue(measure.kind as MeasureKind, v)}`}
                    className="h-6 w-6 rounded-sm"
                    style={{ background: t > 0 ? `rgba(33, 148, 100, ${0.12 + t * 0.88})` : "#f4f4f5" }}
                  />
                );
              })}
            </tr>
          ))}
        </tbody>
      </table>
      <p className="mt-2 text-xs text-zinc-500">Hours are in your account's time zone. Darker means more.</p>
    </div>
  );
}

function Chart({ result, groups, measures }: { result: ReportResult; groups: ReportColumn[]; measures: ReportColumn[] }) {
  const [pick, setPick] = useState(measures[0]?.key ?? "");
  const measure = measures.find((m) => m.key === pick) ?? measures[0];
  if (!measure || groups.length === 0 || result.rows.length === 0) return null;
  const g0 = groups[0].key;
  const heat = groups.length === 2 && g0 === "weekday" && groups[1].key === "hour";
  if (groups.length === 2 && !heat) return null;
  const fmt = (n: number) => formatValue(measure.kind as MeasureKind, n);
  const data = result.rows.map((r) => ({ x: r.keys[0], label: rowLabel(groups, r, 0), v: r.values[measure.key] ?? 0 }));
  const labelOf = new Map(data.map((d) => [d.x, d.label]));
  return (
    <Card
      title={`${measure.label} by ${groups.map((g) => g.label.toLowerCase()).join(" and ")}`}
      actions={
        measures.length > 1 ? (
          <Select value={measure.key} onChange={(e) => setPick(e.target.value)} className="w-48" aria-label="Measure to chart">
            {measures.map((m) => (
              <option key={m.key} value={m.key}>
                {m.label}
              </option>
            ))}
          </Select>
        ) : null
      }
    >
      {heat ? (
        <Heatmap rows={result.rows} measure={measure} />
      ) : TIME_GROUPS.includes(g0) ? (
        <TimeSeriesChart
          data={data}
          xKey="x"
          series={[{ key: "v", label: measure.label }]}
          format={fmt}
          allowDecimals={measure.kind !== "count"}
          xFormat={(k) => (g0 === "month" ? formatBucket("month", k) : formatBucket("day", k))}
        />
      ) : (
        <BarSeriesChart data={data.slice(0, 30)} xKey="x" series={[{ key: "v", label: measure.label }]} format={fmt} xFormat={(k) => labelOf.get(k) ?? k} />
      )}
    </Card>
  );
}

export function ResultView({ result, loading, error }: { result: ReportResult | undefined; loading: boolean; error: string | null }) {
  if (error) return <p className="text-sm text-red-600">{error}</p>;
  const groups = (result?.columns ?? []).filter((c) => c.kind === "group");
  const measures = (result?.columns ?? []).filter((c) => c.kind !== "group");
  const cols = groups.length + measures.length || 4;
  return (
    <div className="space-y-4">
      {result ? <Totals result={result} measures={measures} /> : null}
      {result ? <Chart key={`${groups.map((g) => g.key).join()}:${measures.map((m) => m.key).join()}`} result={result} groups={groups} measures={measures} /> : null}
      {groups.length > 0 || loading ? (
        <Table>
          <thead>
            <tr>
              {groups.map((g) => (
                <Th key={g.key}>{g.label}</Th>
              ))}
              {measures.map((m) => (
                <Th key={m.key} className="text-right">
                  {m.label}
                </Th>
              ))}
            </tr>
          </thead>
          <tbody className={cx(loading && result && "opacity-60")}>
            {loading && !result ? (
              <SkeletonRows cols={cols} />
            ) : result && result.rows.length === 0 ? (
              <EmptyRow cols={cols}>
                <EmptyState icon={<BarChart3 className="size-5" />} title="Nothing in this range" description="Try a longer date range or fewer filters." />
              </EmptyRow>
            ) : (
              result?.rows.map((r, i) => (
                <tr key={`${r.keys.join("|")}-${i}`} className="[&:last-child>td]:border-b-0">
                  {groups.map((g, gi) => (
                    <Td key={g.key} className="max-w-[320px] truncate font-medium text-zinc-900">
                      {rowLabel(groups, r, gi)}
                    </Td>
                  ))}
                  {measures.map((m) => (
                    <Td key={m.key} className="text-right tabular-nums text-zinc-700">
                      {formatValue(m.kind as MeasureKind, r.values[m.key])}
                    </Td>
                  ))}
                </tr>
              ))
            )}
          </tbody>
          {result && result.rows.length > 0 ? (
            <tfoot>
              <tr className="bg-zinc-50/70">
                {groups.map((g, gi) => (
                  <Td key={g.key} className="border-t border-zinc-200 font-semibold text-zinc-900">
                    {gi === 0 ? "Total" : ""}
                  </Td>
                ))}
                {measures.map((m) => (
                  <Td key={m.key} className="border-t border-zinc-200 text-right font-semibold tabular-nums text-zinc-900">
                    {formatValue(m.kind as MeasureKind, result.totals[m.key])}
                  </Td>
                ))}
              </tr>
            </tfoot>
          ) : null}
        </Table>
      ) : null}
      {result?.truncated ? <p className="text-xs text-zinc-500">Showing the first {result.rows.length} rows. Export the CSV or add filters to see the rest.</p> : null}
      {result?.attribution ? <p className="text-xs text-zinc-500">Revenue: {result.attribution} Change this in Settings → Attribution.</p> : null}
    </div>
  );
}
