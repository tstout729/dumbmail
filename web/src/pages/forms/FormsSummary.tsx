import { Link } from "react-router";
import { BarChart3 } from "lucide-react";
import { useFormAnalytics } from "../../api";
import { DeltaBadge, Hint, MiniStat, change } from "../../components/analytics/kit";
import { BarSeriesChart } from "../../components/charts/BarSeriesChart";
import { RangePicker, type RangeDays } from "../../components/charts/RangePicker";
import { Card, Skeleton, cx } from "../../components/ui";
import { formatCompact, formatNumber, formatPercent, pluralize } from "../../lib/format";

const signupColor = "#219464";

/** How all sign-up forms did together over the chosen range, compared with the range before it. */
export function FormsSummary({ days, onDaysChange }: { days: RangeDays; onDaysChange: (d: RangeDays) => void }) {
  const q = useFormAnalytics(days);
  const t = q.data?.totals;
  const p = q.data?.previous;
  const loading = q.isPending;
  const vs = `vs previous ${days} days`;
  const best = q.data?.forms.find((f) => f.submits > 0);

  if (q.error) return <p className="mb-4 text-sm text-red-600">{q.error.message}</p>;

  const quiet = !loading && t && p && !t.views && !t.submits && !p.views && !p.submits;

  return (
    <Card padded={false} className={cx("mb-6 transition-opacity", q.isPlaceholderData && "opacity-60")}>
      <div className="flex flex-wrap items-center justify-between gap-2 border-b border-zinc-100 px-4 py-2.5">
        <h2 className="text-sm font-semibold text-zinc-900">Form performance</h2>
        <RangePicker value={days} onChange={onDaysChange} />
      </div>
      {quiet ? (
        <div className="flex items-center gap-3 px-4 py-5 text-sm text-zinc-500">
          <BarChart3 className="size-4 shrink-0 text-zinc-400" />
          Views and sign-ups show here once a live form gets its first visitor.
        </div>
      ) : (
        <>
          <div className="grid grid-cols-1 md:grid-cols-[minmax(0,260px)_minmax(0,1fr)]">
            <div className="px-4 pt-4 md:pb-4">
              <div className="flex items-center gap-1 text-xs font-medium text-zinc-500">
                Sign-ups from forms
                <Hint text="Every form submission, including people who were already on your list." />
              </div>
              {loading ? (
                <Skeleton className="mt-2 h-9 w-24" />
              ) : (
                <div className="mt-1 flex flex-wrap items-center gap-x-2 gap-y-1">
                  <span className="text-3xl font-semibold tracking-tight text-zinc-950">{formatNumber(t?.submits ?? 0)}</span>
                  <DeltaBadge value={change(t?.submits ?? 0, p?.submits ?? 0, "count")} label={vs} />
                </div>
              )}
              {loading ? (
                <Skeleton className="mt-2 h-4 w-40" />
              ) : (
                <p className="mt-1 text-xs text-zinc-500">
                  From {pluralize(t?.views ?? 0, "view")}
                  {t?.views ? (
                    <>
                      {" · "}
                      <span className="font-medium text-zinc-700">{formatPercent(t.submit_rate, 1)}</span> signed up
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
                series={[{ key: "submits", label: "Sign-ups", color: signupColor }]}
              />
            </div>
          </div>
          <dl className="grid grid-cols-2 border-t border-zinc-100 sm:grid-cols-3 [&>div]:border-zinc-100 max-sm:[&>div:last-child]:col-span-2 max-sm:[&>div:last-child]:border-t sm:[&>div:not(:first-child)]:border-l max-sm:[&>div:nth-child(2)]:border-l">
            <MiniStat
              label="Views"
              value={formatCompact(t?.views ?? 0)}
              title={formatNumber(t?.views ?? 0)}
              delta={change(t?.views ?? 0, p?.views ?? 0, "count")}
              vs={vs}
              loading={loading}
              hint="Times a live form was shown to a visitor."
            />
            <MiniStat
              label="Sign-up rate"
              value={formatPercent(t?.submit_rate ?? 0, 1)}
              // Rates only compare when the earlier range had views; otherwise "+5 pts" is noise.
              delta={(p?.views ?? 0) > 0 ? change(t?.submit_rate ?? 0, p?.submit_rate ?? 0, "rate") : null}
              vs={vs}
              loading={loading}
              hint="Sign-ups, out of views."
            />
            <div className="min-w-0 px-4 py-3">
              <dt className="text-xs font-medium text-zinc-500">Top form</dt>
              <dd className="mt-0.5 min-w-0">
                {loading ? (
                  <Skeleton className="h-6 w-32" />
                ) : best ? (
                  <Link to={`/forms/${best.id}`} className="group block min-w-0">
                    <span className="block truncate text-sm font-semibold text-zinc-950 group-hover:underline">{best.name}</span>
                    <span className="block text-xs text-zinc-500">
                      {pluralize(best.submits, "sign-up")} · {formatPercent(best.submit_rate, 1)}
                    </span>
                  </Link>
                ) : (
                  <span className="text-sm text-zinc-500">No sign-ups yet</span>
                )}
              </dd>
            </div>
          </dl>
        </>
      )}
    </Card>
  );
}
