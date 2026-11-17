import { useState } from "react";
import { useSearchParams } from "react-router";
import { AlertTriangle, CheckCircle2, Info, OctagonAlert } from "lucide-react";
import { errorMessage } from "../../api";
import { RangePicker, parseRange } from "../../components/charts/RangePicker";
import { TimeSeriesChart } from "../../components/charts/TimeSeriesChart";
import { Badge, Card, EmptyRow, Page, PageHeader, Select, Skeleton, SkeletonRows, Table, Td, Th, cx, type Tone } from "../../components/ui";
import { formatNumber, formatPercent } from "../../lib/format";
import { useBenchmarks, useDeliverability } from "./api";
import type { AuthStatus, BenchmarkMetric, Deliverability, Health } from "./types";

const gradeTone: Record<Health["grade"], Tone> = { good: "green", fair: "amber", poor: "red", not_enough_data: "gray" };
const gradeLabel: Record<Health["grade"], string> = { good: "Good", fair: "Fair", poor: "Poor", not_enough_data: "Too few emails" };

function pct(n: number) {
  return formatPercent(n, n !== 0 && n < 0.01 ? 2 : 1);
}

function HealthBadge({ h, score = true }: { h: Health; score?: boolean }) {
  return (
    <Badge tone={gradeTone[h.grade]} dot>
      {score && h.score !== null ? `${h.score} · ` : ""}
      {gradeLabel[h.grade]}
    </Badge>
  );
}

function AuthBadges({ a }: { a: AuthStatus | null }) {
  if (!a) return <span className="text-xs text-zinc-400">Not a verified domain</span>;
  const tone = (s: string): Tone => (s === "pass" ? "green" : s === "pending" ? "amber" : "red");
  return (
    <span className="flex flex-wrap gap-1">
      {(["dkim", "spf", "dmarc"] as const).map((k) => (
        <Badge key={k} tone={tone(a[k])}>
          {k.toUpperCase()} {a[k] === "pass" ? "✓" : a[k]}
        </Badge>
      ))}
    </span>
  );
}

const RATE_CHOICES = [
  { key: "open_rate", label: "Open rate" },
  { key: "click_rate", label: "Click rate" },
  { key: "bounce_rate", label: "Bounce rate" },
  { key: "complaint_rate", label: "Spam complaint rate" },
  { key: "unsubscribe_rate", label: "Unsubscribe rate" },
  { key: "sent", label: "Emails sent" },
] as const;

function OverTime({ data }: { data: Deliverability }) {
  const [metric, setMetric] = useState<(typeof RATE_CHOICES)[number]["key"]>("open_rate");
  const providers = data.providers.filter((p) => p.sent > 0);
  const byDate = new Map<string, Record<string, number | string>>();
  for (const r of data.series) {
    const row = byDate.get(r.date) ?? { date: r.date };
    row[r.provider] = r[metric];
    byDate.set(r.date, row);
  }
  const rows = [...byDate.values()].sort((a, b) => String(a.date).localeCompare(String(b.date)));
  const isRate = metric !== "sent";
  return (
    <Card
      title={`By ${data.interval}`}
      actions={
        <Select value={metric} onChange={(e) => setMetric(e.target.value as typeof metric)} className="w-48" aria-label="Measure">
          {RATE_CHOICES.map((c) => (
            <option key={c.key} value={c.key}>
              {c.label}
            </option>
          ))}
        </Select>
      }
    >
      {rows.length === 0 ? (
        <p className="py-10 text-center text-sm text-zinc-500">No email sent in this range.</p>
      ) : (
        <TimeSeriesChart data={rows} xKey="date" series={providers.map((p) => ({ key: p.provider, label: p.label }))} format={isRate ? pct : formatNumber} allowDecimals={isRate} />
      )}
    </Card>
  );
}

function Flags({ h }: { h: Health }) {
  if (h.grade === "not_enough_data") return <p className="text-sm text-zinc-500">{h.note}</p>;
  if (h.flags.length === 0)
    return (
      <p className="flex items-center gap-1.5 text-sm text-accent-800">
        <CheckCircle2 className="size-4" /> Nothing to fix: bounces, complaints and unsubscribes are all in a healthy range.
      </p>
    );
  return (
    <ul className="space-y-1.5">
      {h.flags.map((f) => (
        <li key={f.text} className={cx("flex items-start gap-1.5 text-sm", f.level === "critical" ? "text-red-700" : f.level === "warning" ? "text-amber-800" : "text-zinc-600")}>
          {f.level === "critical" ? <OctagonAlert className="mt-0.5 size-4 shrink-0" /> : f.level === "warning" ? <AlertTriangle className="mt-0.5 size-4 shrink-0" /> : <Info className="mt-0.5 size-4 shrink-0" />}
          {f.text}
        </li>
      ))}
    </ul>
  );
}

function verdictTone(v: BenchmarkMetric["verdict"]): Tone {
  return v === "better" ? "green" : v === "worse" ? "amber" : "gray";
}

function BenchmarksCard() {
  const [industry, setIndustry] = useState("all");
  const b = useBenchmarks(industry);
  const d = b.data;
  return (
    <Card
      title="How you compare"
      actions={
        <Select value={industry} onChange={(e) => setIndustry(e.target.value)} className="w-56" aria-label="Industry">
          {(d?.industries ?? [{ key: "all", label: "All industries" }]).map((i) => (
            <option key={i.key} value={i.key}>
              {i.label}
            </option>
          ))}
        </Select>
      }
      padded={false}
    >
      {b.error ? <p className="p-4 text-sm text-red-600">{errorMessage(b.error)}</p> : null}
      <div className="overflow-x-auto">
        <table className="w-full min-w-[640px] text-sm">
          <thead>
            <tr className="text-left text-[11px] font-semibold uppercase tracking-wide text-zinc-500">
              <th className="px-4 py-2">Email</th>
              <th className="px-4 py-2 text-right">Last 30 days</th>
              <th className="px-4 py-2 text-right">Your last 90 days</th>
              <th className="px-4 py-2 text-right">Typical</th>
              <th className="px-4 py-2">Published range</th>
            </tr>
          </thead>
          <tbody>
            {!d
              ? Array.from({ length: 5 }, (_, i) => (
                  <tr key={i}>
                    <td className="px-4 py-2.5" colSpan={5}>
                      <Skeleton className="h-4 w-full" />
                    </td>
                  </tr>
                ))
              : d.metrics.map((m) => (
                  <tr key={m.key} className="border-t border-zinc-100">
                    <td className="px-4 py-2.5 font-medium text-zinc-900">{m.label}</td>
                    <td className="px-4 py-2.5 text-right tabular-nums">
                      <span className="inline-flex items-center gap-2">
                        {m.verdict && m.verdict !== "not_enough_data" ? <Badge tone={verdictTone(m.verdict)}>{m.verdict === "better" ? "Better" : "Worse"}</Badge> : null}
                        {pct(m.you)}
                      </span>
                    </td>
                    <td className="px-4 py-2.5 text-right tabular-nums text-zinc-600">{pct(m.trailing_90_days)}</td>
                    <td className="px-4 py-2.5 text-right tabular-nums text-zinc-600">{m.industry !== undefined ? pct(m.industry) : m.limit ? `under ${pct(m.limit.good_below)}` : "—"}</td>
                    <td className="px-4 py-2.5 text-xs text-zinc-500">
                      {m.industry_range ? `${pct(m.industry_range[0])} – ${pct(m.industry_range[1])} across industries` : m.limit ? m.limit.text : "—"}
                    </td>
                  </tr>
                ))}
          </tbody>
        </table>
      </div>
      {d ? (
        <div className="space-y-1 border-t border-zinc-100 px-4 py-3 text-xs text-zinc-500">
          {d.sent_last_30_days < 100 ? <p>Fewer than 100 emails in the last 30 days, so there's no verdict yet.</p> : null}
          <p>
            Opens, clicks and unsubscribes:{" "}
            <a href={d.sources.engagement.url} target="_blank" rel="noreferrer" className="underline hover:text-zinc-800">
              {d.sources.engagement.name}
            </a>
            , published {d.sources.engagement.published}. {d.sources.engagement.period} {d.sources.engagement.note}
          </p>
          <p>
            Bounces and complaints: limits published by{" "}
            {d.metrics
              .filter((m) => m.limit)
              .map((m, i) => (
                <span key={m.key}>
                  {i > 0 ? " and " : ""}
                  <a href={m.limit?.url} target="_blank" rel="noreferrer" className="underline hover:text-zinc-800">
                    {m.limit?.source}
                  </a>{" "}
                  (checked {m.limit?.checked})
                </span>
              ))}
            . {d.note}
          </p>
        </div>
      ) : null}
    </Card>
  );
}

export function DeliverabilityPage() {
  const [params, setParams] = useSearchParams();
  const days = parseRange(params.get("days"));
  const q = useDeliverability(days);
  const d = q.data;
  return (
    <Page>
      <PageHeader
        title="Deliverability"
        description="How your email does at each inbox provider and from each sending domain."
        actions={<RangePicker value={days} onChange={(v) => setParams({ days: String(v) }, { replace: true })} />}
      />
      {q.error ? <p className="mb-4 text-sm text-red-600">{errorMessage(q.error)}</p> : null}
      <div className={cx("space-y-4", q.isPlaceholderData && "opacity-60")}>
        <Card>
          {d ? (
            <div className="grid grid-cols-1 gap-4 md:grid-cols-[220px_minmax(0,1fr)]">
              <div>
                <div className="text-xs font-medium text-zinc-500">Health</div>
                <div className="mt-1 flex items-center gap-2">
                  <span className="text-4xl font-semibold tracking-tight text-zinc-950">{d.overall.health.score ?? "—"}</span>
                  <HealthBadge h={d.overall.health} score={false} />
                </div>
                <div className="mt-2 text-xs text-zinc-500">
                  {formatNumber(d.overall.sent)} emails · {pct(d.overall.delivery_rate)} delivered
                </div>
              </div>
              <Flags h={d.overall.health} />
            </div>
          ) : (
            <Skeleton className="h-16 w-full" />
          )}
        </Card>
        <div>
          <h2 className="mb-2 text-sm font-semibold text-zinc-900">Inbox providers</h2>
          <Table>
            <thead>
              <tr>
                <Th>Provider</Th>
                <Th className="text-right">Sent</Th>
                <Th className="text-right">Delivered</Th>
                <Th className="text-right">Opens</Th>
                <Th className="text-right">Clicks</Th>
                <Th className="text-right">Hard / soft bounces</Th>
                <Th className="text-right">Complaints</Th>
                <Th className="text-right">Unsubscribes</Th>
                <Th>Health</Th>
              </tr>
            </thead>
            <tbody>
              {!d ? (
                <SkeletonRows cols={9} />
              ) : (
                d.providers.map((p) => (
                  <tr key={p.provider} className="align-top [&:last-child>td]:border-b-0">
                    <Td className="font-medium text-zinc-900">
                      {p.label}
                      {p.health.flags.length > 0 ? (
                        <ul className="mt-1 max-w-xs space-y-0.5 text-xs font-normal text-amber-800">
                          {p.health.flags.map((f) => (
                            <li key={f.text}>{f.text}</li>
                          ))}
                        </ul>
                      ) : null}
                    </Td>
                    <Td className="text-right tabular-nums">{formatNumber(p.sent)}</Td>
                    <Td className="text-right tabular-nums">{pct(p.delivery_rate)}</Td>
                    <Td className="text-right tabular-nums">{pct(p.open_rate)}</Td>
                    <Td className="text-right tabular-nums">{pct(p.click_rate)}</Td>
                    <Td className="text-right tabular-nums">
                      {pct(p.hard_bounce_rate)} / {pct(p.soft_bounce_rate)}
                    </Td>
                    <Td className="text-right tabular-nums">{pct(p.complaint_rate)}</Td>
                    <Td className="text-right tabular-nums">{pct(p.unsubscribe_rate)}</Td>
                    <Td>
                      <HealthBadge h={p.health} />
                    </Td>
                  </tr>
                ))
              )}
            </tbody>
          </Table>
          {d ? <p className="mt-2 text-xs text-zinc-500">{d.provider_note}</p> : null}
        </div>
        {d ? <OverTime data={d} /> : null}
        <div>
          <h2 className="mb-2 text-sm font-semibold text-zinc-900">Sending domains</h2>
          <Table>
            <thead>
              <tr>
                <Th>Domain</Th>
                <Th className="text-right">Sent</Th>
                <Th className="text-right">Opens</Th>
                <Th className="text-right">Bounces</Th>
                <Th className="text-right">Complaints</Th>
                <Th>Authentication</Th>
                <Th>Health</Th>
              </tr>
            </thead>
            <tbody>
              {!d ? (
                <SkeletonRows cols={7} rows={2} />
              ) : d.sending_domains.length === 0 ? (
                <EmptyRow cols={7}>
                  <p className="px-4 py-6 text-center text-sm text-zinc-500">No email sent in this range.</p>
                </EmptyRow>
              ) : (
                d.sending_domains.map((s) => (
                  <tr key={s.label} className="[&:last-child>td]:border-b-0">
                    <Td className="font-medium text-zinc-900">{s.label}</Td>
                    <Td className="text-right tabular-nums">{formatNumber(s.sent)}</Td>
                    <Td className="text-right tabular-nums">{pct(s.open_rate)}</Td>
                    <Td className="text-right tabular-nums">{pct(s.bounce_rate)}</Td>
                    <Td className="text-right tabular-nums">{pct(s.complaint_rate)}</Td>
                    <Td>{s.domain ? <AuthBadges a={s.authentication} /> : <span className="text-xs text-zinc-400">—</span>}</Td>
                    <Td>
                      <HealthBadge h={s.health} />
                    </Td>
                  </tr>
                ))
              )}
            </tbody>
          </Table>
          {d && d.authentication.length > 0 ? (
            <div className="mt-3 flex flex-wrap gap-x-6 gap-y-2 text-xs text-zinc-600">
              {d.authentication.map((a) => (
                <span key={a.domain} className="flex items-center gap-2">
                  <span className="font-medium text-zinc-800">{a.domain}</span>
                  <AuthBadges a={a} />
                </span>
              ))}
              <span className="text-zinc-500">From the last check in Settings → Domains.</span>
            </div>
          ) : null}
        </div>
        <BenchmarksCard />
      </div>
    </Page>
  );
}
