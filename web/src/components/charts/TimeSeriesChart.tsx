import { Area, AreaChart, CartesianGrid, Legend, ResponsiveContainer, Tooltip, XAxis, YAxis } from "recharts";
import { formatShortDate } from "../../lib/format";
import { Skeleton } from "../ui";

/** Validated categorical order (dataviz validator, light surface). Assigned in fixed order, never cycled. */
export const seriesColors = ["#219464", "#6d5bd0", "#d97706", "#0284c7", "#db2777", "#4d7c0f"];

export type Series = { key: string; label: string; color?: string };

type Props = {
  data: Record<string, unknown>[];
  xKey: string;
  series: Series[];
  format?: (n: number) => string;
  height?: number;
  loading?: boolean;
  xFormat?: (v: string) => string;
  stacked?: boolean;
  /** Allow fractional Y ticks, e.g. for small dollar amounts. */
  allowDecimals?: boolean;
  /** For callers that draw their own legend. */
  hideLegend?: boolean;
};

export function TimeSeriesChart({ data, xKey, series, format = (n) => n.toLocaleString(), height = 240, loading, xFormat, stacked, allowDecimals = false, hideLegend }: Props) {
  if (loading)
    return (
      <div style={{ height }}>
        <Skeleton className="h-full w-full" />
      </div>
    );
  const fmtX = xFormat ?? ((v: string) => formatShortDate(v));
  return (
    <div style={{ height }} className="w-full">
      <ResponsiveContainer width="100%" height="100%">
        <AreaChart data={data} margin={{ top: 8, right: 8, bottom: 0, left: 0 }}>
          <defs>
            {series.map((s, i) => {
              const color = s.color ?? seriesColors[i];
              return (
                <linearGradient key={s.key} id={`fill-${s.key}`} x1="0" y1="0" x2="0" y2="1">
                  <stop offset="0%" stopColor={color} stopOpacity={0.18} />
                  <stop offset="100%" stopColor={color} stopOpacity={0} />
                </linearGradient>
              );
            })}
          </defs>
          <CartesianGrid vertical={false} stroke="#ececef" />
          <XAxis
            dataKey={xKey}
            tickFormatter={(v: string) => fmtX(String(v))}
            tick={{ fontSize: 11, fill: "#71717a" }}
            tickLine={false}
            axisLine={{ stroke: "#e4e4e7" }}
            minTickGap={24}
          />
          <YAxis
            tickFormatter={(v: number) => format(v)}
            tick={{ fontSize: 11, fill: "#71717a" }}
            tickLine={false}
            axisLine={false}
            width={56}
            allowDecimals={allowDecimals}
          />
          <Tooltip
            cursor={{ stroke: "#a1a1aa", strokeDasharray: "3 3" }}
            contentStyle={{ borderRadius: 8, border: "1px solid #e4e4e7", fontSize: 12, boxShadow: "0 4px 12px rgb(0 0 0 / 0.08)" }}
            labelStyle={{ color: "#18181b", fontWeight: 600, marginBottom: 2 }}
            itemStyle={{ color: "#3f3f46", padding: 0 }}
            labelFormatter={(v) => fmtX(String(v))}
            formatter={(value) => format(Number(value))}
          />
          {series.length > 1 && !hideLegend ? <Legend verticalAlign="top" align="right" iconType="circle" iconSize={8} height={28} wrapperStyle={{ fontSize: 12, color: "#3f3f46" }} /> : null}
          {series.map((s, i) => (
            <Area
              key={s.key}
              type="monotone"
              dataKey={s.key}
              name={s.label}
              stroke={s.color ?? seriesColors[i]}
              strokeWidth={2}
              fill={`url(#fill-${s.key})`}
              stackId={stacked ? "stack" : undefined}
              dot={false}
              activeDot={{ r: 4, strokeWidth: 2, stroke: "#fff" }}
              isAnimationActive={false}
            />
          ))}
        </AreaChart>
      </ResponsiveContainer>
    </div>
  );
}
