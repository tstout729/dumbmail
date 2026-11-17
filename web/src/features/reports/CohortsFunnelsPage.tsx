import { useState } from "react";
import { useSearchParams } from "react-router";
import { Download, Plus, X } from "lucide-react";
import { errorMessage, useMetrics } from "../../api";
import { Button, Card, Field, IconButton, Input, Page, PageHeader, Segmented, Select, Skeleton, Tabs, cx } from "../../components/ui";
import { formatCurrency, formatNumber, formatPercent } from "../../lib/format";
import { toast } from "../../lib/toast";
import { downloadCsv, useCohorts, useFunnel } from "./api";
import type { FunnelQuery, FunnelSplit, FunnelStep } from "./types";

const monthFmt = new Intl.DateTimeFormat("en-US", { month: "short", year: "numeric", timeZone: "UTC" });

function cohortLabel(ym: string) {
  const [y, m] = ym.split("-").map(Number);
  return monthFmt.format(new Date(Date.UTC(y, m - 1, 1)));
}

function Cohorts() {
  const [by, setBy] = useState<"first_purchase" | "signup">("first_purchase");
  const [months, setMonths] = useState("12");
  const [view, setView] = useState<"rate" | "revenue">("rate");
  const [downloading, setDownloading] = useState(false);
  const q = useCohorts(by, Number(months));
  const d = q.data;
  const width = Math.max(0, ...(d?.cohorts ?? []).map((c) => c.cells.length));
  const maxRate = Math.max(0.0001, ...(d?.cohorts ?? []).flatMap((c) => c.cells.slice(by === "first_purchase" ? 0 : 0).map((x) => x.rate)));
  const maxRev = Math.max(0.01, ...(d?.cohorts ?? []).flatMap((c) => c.cells.map((x) => x.cumulative_revenue_per_person)));
  return (
    <div className="space-y-4">
      <div className="flex flex-wrap items-center gap-3">
        <Segmented
          value={by}
          onChange={setBy}
          options={[
            { value: "first_purchase", label: "By first purchase" },
            { value: "signup", label: "By sign-up" },
          ]}
        />
        <Segmented
          value={view}
          onChange={setView}
          options={[
            { value: "rate", label: by === "signup" ? "Bought" : "Bought again" },
            { value: "revenue", label: "Revenue per person" },
          ]}
        />
        <Select value={months} onChange={(e) => setMonths(e.target.value)} className="w-40" aria-label="Months">
          <option value="6">Last 6 months</option>
          <option value="12">Last 12 months</option>
          <option value="24">Last 24 months</option>
        </Select>
        <Button
          icon={<Download className="size-4" />}
          loading={downloading}
          onClick={async () => {
            setDownloading(true);
            try {
              await downloadCsv(`/api/reports/cohorts?by=${by}&months=${months}&format=csv`, undefined, "cohorts.csv");
            } catch (e) {
              toast.error(errorMessage(e));
            } finally {
              setDownloading(false);
            }
          }}
        >
          Export CSV
        </Button>
      </div>
      {q.error ? <p className="text-sm text-red-600">{errorMessage(q.error)}</p> : null}
      <Card padded={false}>
        {!d ? (
          <div className="p-4">
            <Skeleton className="h-48 w-full" />
          </div>
        ) : d.cohorts.length === 0 ? (
          <p className="px-4 py-12 text-center text-sm text-zinc-500">
            {by === "first_purchase" ? `No ${d.metrics.join(" or ")} events in this range yet.` : "No new profiles in this range."}
          </p>
        ) : (
          <div className={cx("overflow-x-auto", q.isPlaceholderData && "opacity-60")}>
            <table className="min-w-full border-separate border-spacing-0 text-xs">
              <thead>
                <tr className="text-left text-[11px] font-semibold uppercase tracking-wide text-zinc-500">
                  <th className="sticky left-0 bg-white px-3 py-2">Cohort</th>
                  <th className="px-3 py-2 text-right">People</th>
                  {Array.from({ length: width }, (_, k) => (
                    <th key={k} className="px-2 py-2 text-center">
                      Month {k}
                    </th>
                  ))}
                </tr>
              </thead>
              <tbody>
                {d.cohorts.map((c) => (
                  <tr key={c.cohort}>
                    <td className="sticky left-0 whitespace-nowrap border-t border-zinc-100 bg-white px-3 py-1.5 font-medium text-zinc-900">{cohortLabel(c.cohort)}</td>
                    <td className="border-t border-zinc-100 px-3 py-1.5 text-right tabular-nums text-zinc-700">{formatNumber(c.size)}</td>
                    {Array.from({ length: width }, (_, k) => {
                      const cell = c.cells[k];
                      if (!cell) return <td key={k} className="border-t border-zinc-100" />;
                      const t = view === "rate" ? cell.rate / maxRate : cell.cumulative_revenue_per_person / maxRev;
                      return (
                        <td
                          key={k}
                          className="border-t border-zinc-100 px-2 py-1.5 text-center tabular-nums"
                          style={{ background: t > 0 ? `rgba(33, 148, 100, ${0.08 + t * 0.6})` : undefined, color: t > 0.65 ? "white" : undefined }}
                          title={`${formatNumber(cell.customers)} people · ${formatCurrency(cell.revenue)} that month`}
                        >
                          {view === "rate" ? formatPercent(cell.rate, 1) : formatCurrency(cell.cumulative_revenue_per_person, true)}
                        </td>
                      );
                    })}
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        )}
      </Card>
      {d ? (
        <p className="text-xs text-zinc-500">
          {view === "rate"
            ? `${d.rate_label}: the share of each cohort who bought in each month after it started.`
            : "Total revenue from each cohort so far, divided by its size."}{" "}
          Months are calendar months in {d.timezone}. Purchases are {d.metrics.join(" or ")} events.
        </p>
      ) : null}
    </div>
  );
}

const DEFAULT_STEPS = ["Viewed Product", "Added to Cart", "Started Checkout", "Placed Order"];

function StepBars({ steps }: { steps: FunnelStep[] }) {
  const first = steps[0]?.people ?? 0;
  return (
    <ol className="space-y-2">
      {steps.map((s) => (
        <li key={s.step}>
          <div className="mb-1 flex flex-wrap items-baseline justify-between gap-2 text-sm">
            <span className="font-medium text-zinc-900">
              {s.step}. {s.metric}
            </span>
            <span className="tabular-nums text-zinc-600">
              {formatNumber(s.people)} people
              {s.step > 1 ? (
                <>
                  {" "}
                  · {formatPercent(s.conversion_from_previous)} of the step before · <span className="text-zinc-400">{formatNumber(s.dropped)} dropped</span>
                </>
              ) : null}
            </span>
          </div>
          <div className="h-6 overflow-hidden rounded bg-zinc-100">
            <div className="h-full rounded bg-accent-600" style={{ width: `${first > 0 ? (s.people / first) * 100 : 0}%` }} />
          </div>
        </li>
      ))}
    </ol>
  );
}

function Funnels() {
  const metrics = useMetrics();
  const names = (metrics.data ?? []).map((m) => m.name);
  const [steps, setSteps] = useState<string[]>(DEFAULT_STEPS);
  const [windowDays, setWindowDays] = useState("7");
  const [days, setDays] = useState("30");
  const [splitType, setSplitType] = useState<"" | FunnelSplit["type"]>("");
  const [splitKey, setSplitKey] = useState("");
  const [query, setQuery] = useState<FunnelQuery | null>(null);
  const f = useFunnel(query);
  const options = [...new Set([...names, ...steps])];
  const run = () => {
    const split: FunnelSplit | undefined = splitType === "" ? undefined : splitType === "source" ? { type: "source" } : { type: splitType, key: splitKey.trim() };
    setQuery({ steps: steps.filter(Boolean), window_days: Number(windowDays), range: { days: Number(days) }, split_by: split });
  };
  const canRun = steps.filter(Boolean).length >= 2 && (splitType === "" || splitType === "source" || splitKey.trim() !== "");
  return (
    <div className="grid grid-cols-1 gap-4 lg:grid-cols-[320px_minmax(0,1fr)]">
      <Card className="h-fit">
        <div className="space-y-4">
          <Field label="Steps, in order">
            <div className="space-y-1.5">
              {steps.map((s, i) => (
                <div key={i} className="flex items-center gap-1.5">
                  <span className="w-4 text-xs text-zinc-500">{i + 1}</span>
                  <Select value={s} onChange={(e) => setSteps(steps.map((x, j) => (j === i ? e.target.value : x)))} aria-label={`Step ${i + 1}`}>
                    <option value="">Pick a metric</option>
                    {options.map((n) => (
                      <option key={n} value={n}>
                        {n}
                      </option>
                    ))}
                  </Select>
                  <IconButton label="Remove step" size="sm" disabled={steps.length <= 2} onClick={() => setSteps(steps.filter((_, j) => j !== i))}>
                    <X className="size-3.5" />
                  </IconButton>
                </div>
              ))}
              {steps.length < 6 ? (
                <Button size="sm" variant="ghost" icon={<Plus className="size-3.5" />} onClick={() => setSteps([...steps, ""])}>
                  Add step
                </Button>
              ) : null}
            </div>
          </Field>
          <div className="grid grid-cols-1 gap-4">
            <Field label="First step in">
              <Select value={days} onChange={(e) => setDays(e.target.value)}>
                <option value="7">Last 7 days</option>
                <option value="30">Last 30 days</option>
                <option value="90">Last 90 days</option>
                <option value="365">Last 12 months</option>
              </Select>
            </Field>
            <Field label="Finish within">
              <Select value={windowDays} onChange={(e) => setWindowDays(e.target.value)}>
                <option value="1">1 day</option>
                <option value="3">3 days</option>
                <option value="7">7 days</option>
                <option value="14">14 days</option>
                <option value="30">30 days</option>
              </Select>
            </Field>
          </div>
          <Field label="Split by">
            <div className="space-y-1.5">
              <Select value={splitType} onChange={(e) => setSplitType(e.target.value as typeof splitType)}>
                <option value="">Nothing</option>
                <option value="source">Came from a campaign or flow</option>
                <option value="event_property">A property of the first step's event</option>
                <option value="profile_property">A profile property</option>
              </Select>
              {splitType === "event_property" || splitType === "profile_property" ? (
                <Input value={splitKey} onChange={(e) => setSplitKey(e.target.value)} placeholder="Property name" aria-label="Property name" />
              ) : null}
            </div>
          </Field>
          <Button variant="primary" className="w-full" disabled={!canRun} loading={f.isFetching} onClick={run}>
            Show funnel
          </Button>
        </div>
      </Card>
      <div className="min-w-0 space-y-4">
        {f.error ? <p className="text-sm text-red-600">{errorMessage(f.error)}</p> : null}
        {!query ? (
          <Card>
            <p className="py-10 text-center text-sm text-zinc-500">Pick the steps people take, then show the funnel.</p>
          </Card>
        ) : !f.data ? (
          <Card>
            <Skeleton className="h-40 w-full" />
          </Card>
        ) : (
          <>
            <Card title="Everyone">
              {f.data.total[0]?.people ? <StepBars steps={f.data.total} /> : <p className="py-6 text-center text-sm text-zinc-500">Nobody did the first step in this range.</p>}
            </Card>
            {f.data.splits.length > 0 ? (
              <div className="grid grid-cols-1 gap-4 xl:grid-cols-2">
                {f.data.splits.map((s) => (
                  <Card key={s.key} title={s.label}>
                    <StepBars steps={s.steps} />
                  </Card>
                ))}
              </div>
            ) : null}
            <p className="text-xs text-zinc-500">
              Each person counts once, from their first {f.data.steps[0]} in the range. Later steps count only in order and within {f.data.window_days} day
              {f.data.window_days === 1 ? "" : "s"} of it.
            </p>
          </>
        )}
      </div>
    </div>
  );
}

export function CohortsFunnelsPage() {
  const [params, setParams] = useSearchParams();
  const tab = params.get("tab") === "funnels" ? "funnels" : "cohorts";
  return (
    <Page wide>
      <PageHeader title="Cohorts & funnels" description="Who comes back to buy again, and where people drop out on the way to buying." />
      <Tabs
        value={tab}
        onChange={(v) => setParams({ tab: v }, { replace: true })}
        tabs={[
          { value: "cohorts", label: "Cohorts" },
          { value: "funnels", label: "Funnels" },
        ]}
      />
      {tab === "cohorts" ? <Cohorts /> : <Funnels />}
    </Page>
  );
}
