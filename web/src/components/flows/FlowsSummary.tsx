import { BarChart3 } from "lucide-react";
import { useFlowAnalytics, type FlowTotals } from "../../api";
import { DeltaBadge, Hint, MiniStat, change } from "../analytics/kit";
import { BarSeriesChart } from "../charts/BarSeriesChart";
import { RangePicker, type RangeDays } from "../charts/RangePicker";
import { Card, Skeleton, cx } from "../ui";
import { formatCompact, formatCurrency, formatNumber, formatPercent, pluralize } from "../../lib/format";

const flowColor = "#219464";

/** How all flows did together over the chosen range, compared with the range before it. */
export function FlowsSummary({ days, onDaysChange }: { days: RangeDays; onDaysChange: (d: RangeDays) => void }) {
  const q = useFlowAnalytics(days);
  const t = q.data?.totals;
  const p = q.data?.previous;
  const loading = q.isPending;
  const vs = `vs previous ${days} days`;
  // Rates only compare when the earlier range actually sent something; otherwise "+40 pts" is noise.
  const ratesComparable = (p?.messages_sent ?? 0) > 0;
  const share = t && t.store_revenue > 0 ? t.revenue / t.store_revenue : null;

  if (q.error) return <p className="mb-4 text-sm text-red-600">{q.error.message}</p>;

  const quiet = !loading && t && p && !t.messages_sent && !t.revenue && !p.messages_sent && !p.revenue;

  return (
    <Card padded={false} className={cx("mb-6 transition-opacity", q.isPlaceholderData && "opacity-60")}>
      <div className="flex flex-wrap items-center justify-between gap-2 border-b border-zinc-100 px-4 py-2.5">
        <h2 className="text-sm font-semibold text-zinc-900">Flow performance</h2>
        <RangePicker value={days} onChange={onDaysChange} />
      </div>
      {quiet ? (
        <div className="flex items-center gap-3 px-4 py-5 text-sm text-zinc-500">
          <BarChart3 className="size-4 shrink-0 text-zinc-400" />
          Revenue, opens and clicks show here once a live flow sends its first message.
        </div>
      ) : (
        <>
          <div className="grid grid-cols-1 md:grid-cols-[minmax(0,260px)_minmax(0,1fr)]">
            <div className="px-4 pt-4 md:pb-4">
              <div className="flex items-center gap-1 text-xs font-medium text-zinc-500">
                Revenue from flows
                <Hint text="Orders placed within 5 days of opening or clicking a flow email, or 24 hours of clicking a flow text. The newest message gets the credit." />
              </div>
              {loading ? (
                <Skeleton className="mt-2 h-9 w-32" />
              ) : (
                <div className="mt-1 flex flex-wrap items-center gap-x-2 gap-y-1">
                  <span className="text-3xl font-semibold tracking-tight text-zinc-950">{formatCurrency(t?.revenue)}</span>
                  <DeltaBadge value={change(t?.revenue ?? 0, p?.revenue ?? 0, "count")} label={vs} />
                </div>
              )}
              {loading ? (
                <Skeleton className="mt-2 h-4 w-44" />
              ) : (
                <p className="mt-1 text-xs text-zinc-500">
                  {pluralize(t?.orders ?? 0, "order")}
                  {share !== null ? (
                    <>
                      {" · "}
                      <span className="font-medium text-zinc-700">{formatPercent(share, 0)}</span> of store revenue
                    </>
                  ) : null}
                </p>
              )}
            </div>
            <div className="min-w-0 px-2 pt-3 pb-2 md:pr-4">
              <BarSeriesChart
                loading={loading}
                data={q.data?.series ?? []}
                xKey="date"
                height={110}
                format={(n) => formatCurrency(n)}
                series={[{ key: "revenue", label: "Flow revenue", color: flowColor }]}
              />
            </div>
          </div>
          <dl className="grid grid-cols-2 border-t border-zinc-100 sm:grid-cols-4 [&>div]:border-zinc-100 max-sm:[&>div:nth-child(-n+2)]:border-b sm:[&>div:not(:first-child)]:border-l max-sm:[&>div:nth-child(even)]:border-l">
            <MiniStat
              label="Messages sent"
              value={formatCompact(t?.messages_sent ?? 0)}
              title={formatNumber(t?.messages_sent ?? 0)}
              delta={change(t?.messages_sent ?? 0, p?.messages_sent ?? 0, "count")}
              vs={vs}
              loading={loading}
            />
            <RateStat label="Open rate" k="open_rate" t={t} p={ratesComparable ? p : undefined} vs={vs} loading={loading} hint="Emails opened, out of emails delivered." />
            <RateStat label="Click rate" k="click_rate" t={t} p={ratesComparable ? p : undefined} vs={vs} loading={loading} hint="Emails and texts clicked, out of those delivered." />
            <MiniStat
              label="Revenue per recipient"
              value={formatCurrency(t?.revenue_per_recipient, true)}
              delta={(p?.recipients ?? 0) > 0 ? change(t?.revenue_per_recipient ?? 0, p?.revenue_per_recipient ?? 0, "count") : null}
              vs={vs}
              loading={loading}
              hint={`Flow revenue divided by the ${formatNumber(t?.recipients ?? 0)} people your flows messaged.`}
            />
          </dl>
        </>
      )}
    </Card>
  );
}

function RateStat({ label, k, t, p, vs, loading, hint }: { label: string; k: "open_rate" | "click_rate"; t?: FlowTotals; p?: FlowTotals; vs: string; loading: boolean; hint: string }) {
  return (
    <MiniStat label={label} value={formatPercent(t?.[k] ?? 0, 1)} delta={p ? change(t?.[k] ?? 0, p[k], "rate") : null} vs={vs} loading={loading} hint={hint} />
  );
}
