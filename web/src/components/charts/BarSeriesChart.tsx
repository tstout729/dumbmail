import { Bar, BarChart, CartesianGrid, ResponsiveContainer, Tooltip, XAxis, YAxis, type TooltipContentProps } from "recharts";
import type { NameType, ValueType } from "recharts/types/component/DefaultTooltipContent";
import { formatShortDate } from "../../lib/format";
import { Skeleton } from "../ui";
import { seriesColors, type Series } from "./TimeSeriesChart";

type Props = {
  data: Record<string, unknown>[];
  xKey: string;
  series: Series[];
  format?: (n: number) => string;
  height?: number;
  loading?: boolean;
  xFormat?: (v: string) => string;
  /** Adds a "Total" line to the tooltip when bars are stacked. */
  showTotal?: boolean;
};

function ChartTooltip({ active, payload, label, fmtX, format, showTotal }: TooltipContentProps<ValueType, NameType> & { fmtX: (v: string) => string; format: (n: number) => string; showTotal?: boolean }) {
  if (!active || !payload?.length) return null;
  const total = payload.reduce((s, p) => s + Number(p.value ?? 0), 0);
  return (
    <div className="min-w-40 rounded-lg border border-zinc-200 bg-white px-3 py-2 text-xs shadow-lg">
      <div className="mb-1 font-semibold text-zinc-900">{fmtX(String(label))}</div>
      {[...payload].reverse().map((p) => (
        <div key={String(p.dataKey)} className="flex items-center justify-between gap-4 py-0.5">
          <span className="flex items-center gap-1.5 text-zinc-600">
            <span className="size-2 rounded-full" style={{ background: p.color }} />
            {p.name}
          </span>
          <span className="font-medium tabular-nums text-zinc-900">{format(Number(p.value ?? 0))}</span>
        </div>
      ))}
      {showTotal && payload.length > 1 ? (
        <div className="mt-1 flex justify-between gap-4 border-t border-zinc-100 pt-1">
          <span className="text-zinc-600">Total</span>
          <span className="font-semibold tabular-nums text-zinc-900">{format(total)}</span>
        </div>
      ) : null}
    </div>
  );
}

/** Stacked columns over time. Legends live outside the chart so the caller can place them. */
export function BarSeriesChart({ data, xKey, series, format = (n) => n.toLocaleString(), height = 240, loading, xFormat, showTotal }: Props) {
  if (loading)
    return (
      <div style={{ height }}>
        <Skeleton className="h-full w-full" />
      </div>
    );
  const fmtX = xFormat ?? ((v: string) => formatShortDate(v));
  const top = series.length - 1;

  return (
    <div style={{ height }} className="w-full">
      <ResponsiveContainer width="100%" height="100%">
        <BarChart data={data} margin={{ top: 8, right: 4, bottom: 0, left: 0 }} barCategoryGap={data.length > 45 ? 1 : "22%"}>
          <CartesianGrid vertical={false} stroke="#ececef" />
          <XAxis
            dataKey={xKey}
            tickFormatter={(v: string) => fmtX(String(v))}
            tick={{ fontSize: 11, fill: "#71717a" }}
            tickLine={false}
            axisLine={{ stroke: "#e4e4e7" }}
            minTickGap={28}
          />
          <YAxis tickFormatter={(v: number) => format(v)} tick={{ fontSize: 11, fill: "#71717a" }} tickLine={false} axisLine={false} width={52} allowDecimals={false} />
          <Tooltip cursor={{ fill: "#f4f4f5" }} content={(p) => <ChartTooltip {...p} fmtX={fmtX} format={format} showTotal={showTotal} />} />
          {series.map((s, i) => (
            <Bar
              key={s.key}
              dataKey={s.key}
              name={s.label}
              stackId="stack"
              fill={s.color ?? seriesColors[i]}
              radius={i === top ? [3, 3, 0, 0] : 0}
              isAnimationActive={false}
              maxBarSize={28}
            />
          ))}
        </BarChart>
      </ResponsiveContainer>
    </div>
  );
}
