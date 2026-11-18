import { useState } from "react";
import { errorMessage, useMetrics } from "../../api";
import { Button, Card, Field, Notice, Segmented, Select, Skeleton, TagInput, Toggle } from "../../components/ui";
import { useDebouncedValue } from "../../lib/hooks";
import { formatCurrency, formatNumber } from "../../lib/format";
import { toast } from "../../lib/toast";
import { useAttribution, useAttributionPreview, useSaveAttribution } from "./api";
import type { AttributionPatch, AttributionSettings, AttributionTotals } from "./types";

const WINDOWS = [
  { hours: 1, label: "1 hour" },
  { hours: 6, label: "6 hours" },
  { hours: 12, label: "12 hours" },
  { hours: 24, label: "1 day" },
  { hours: 48, label: "2 days" },
  { hours: 72, label: "3 days" },
  { hours: 120, label: "5 days" },
  { hours: 168, label: "7 days" },
  { hours: 336, label: "14 days" },
  { hours: 720, label: "30 days" },
];

function WindowSelect({ value, onChange, label, disabled }: { value: number; onChange: (h: number) => void; label: string; disabled?: boolean }) {
  const options = WINDOWS.some((w) => w.hours === value) ? WINDOWS : [...WINDOWS, { hours: value, label: `${value} hours` }].sort((a, b) => a.hours - b.hours);
  return (
    <Select value={value} onChange={(e) => onChange(Number(e.target.value))} aria-label={label} disabled={disabled}>
      {options.map((w) => (
        <option key={w.hours} value={w.hours}>
          {w.label}
        </option>
      ))}
    </Select>
  );
}

export function AttributionSettingsPage() {
  const q = useAttribution();
  if (q.error) return <p className="text-sm text-red-600">{errorMessage(q.error)}</p>;
  if (!q.data) return <Skeleton className="h-72 w-full" />;
  return <Editor key={JSON.stringify(q.data)} saved={q.data} />;
}

function same(a: AttributionPatch, b: AttributionSettings) {
  return (
    a.email_open_window_hours === b.email_open_window_hours &&
    a.email_click_window_hours === b.email_click_window_hours &&
    a.sms_click_window_hours === b.sms_click_window_hours &&
    a.count_opens === b.count_opens &&
    a.model === b.model &&
    JSON.stringify(a.conversion_metrics) === JSON.stringify(b.conversion_metrics)
  );
}

function Editor({ saved }: { saved: AttributionSettings }) {
  const [draft, setDraft] = useState<Required<AttributionPatch>>({
    email_open_window_hours: saved.email_open_window_hours,
    email_click_window_hours: saved.email_click_window_hours,
    sms_click_window_hours: saved.sms_click_window_hours,
    count_opens: saved.count_opens,
    model: saved.model,
    conversion_metrics: saved.conversion_metrics,
  });
  const [days, setDays] = useState<"30" | "90">("30");
  const metrics = useMetrics();
  const save = useSaveAttribution();
  const changed = !same(draft, saved);
  const debounced = useDebouncedValue(draft, 400);
  const preview = useAttributionPreview(changed && debounced.conversion_metrics.length > 0 ? debounced : null, Number(days));
  const set = (p: AttributionPatch) => setDraft((d) => ({ ...d, ...p }));
  const known = (metrics.data ?? []).map((m) => m.name);

  return (
    <div className="space-y-4">
      <Card title="Attribution">
        <p className="mb-4 text-sm text-zinc-600">
          How DumbMail decides which email or text earned an order. Every report uses this, including the overview, campaign and flow reports, and past
          dates.
        </p>
        <div className="grid grid-cols-1 gap-4 sm:grid-cols-2">
          <Field label="What counts as a conversion" hint={known.length ? `Metrics you track: ${known.slice(0, 8).join(", ")}${known.length > 8 ? "…" : ""}` : "Metric names, then Enter."}>
            <TagInput value={draft.conversion_metrics} onChange={(v) => set({ conversion_metrics: v.slice(0, 5) })} placeholder="Placed Order" aria-label="Conversion metrics" />
          </Field>
          <Field label="Who gets the credit">
            <Segmented
              value={draft.model}
              onChange={(v) => set({ model: v })}
              options={[
                { value: "last_touch", label: "Last message touched" },
                { value: "first_touch", label: "First message touched" },
              ]}
            />
          </Field>
          <Field label="Email clicked within">
            <WindowSelect value={draft.email_click_window_hours} onChange={(h) => set({ email_click_window_hours: h })} label="Email click window" />
          </Field>
          <Field label="Email opened within" hint={draft.count_opens ? undefined : "Off: only clicks count."}>
            <div className="flex items-center gap-3">
              <WindowSelect value={draft.email_open_window_hours} onChange={(h) => set({ email_open_window_hours: h })} label="Email open window" disabled={!draft.count_opens} />
              <Toggle checked={draft.count_opens} onChange={(v) => set({ count_opens: v })} label="Count opens" />
            </div>
          </Field>
          <Field label="Text clicked within">
            <WindowSelect value={draft.sms_click_window_hours} onChange={(h) => set({ sms_click_window_hours: h })} label="SMS click window" />
          </Field>
        </div>
        <p className="mt-4 text-xs text-zinc-500">
          Apple Mail opens many emails on its own, so opens can give email credit for orders it didn't drive. Turning opens off counts clicks only.
        </p>
        <div className="mt-4 flex flex-wrap items-center justify-end gap-2">
          {changed ? (
            <Button variant="ghost" onClick={() => setDraft({ ...saved })}>
              Undo changes
            </Button>
          ) : null}
          <Button
            variant="primary"
            disabled={!changed || draft.conversion_metrics.length === 0}
            loading={save.isPending}
            onClick={() => save.mutate(draft, { onSuccess: () => toast.success("Attribution saved. Reports use it now.") })}
          >
            Save
          </Button>
        </div>
      </Card>
      {changed ? (
        <Card
          title="What this changes"
          actions={
            <Segmented
              size="sm"
              value={days}
              onChange={setDays}
              options={[
                { value: "30", label: "30 days" },
                { value: "90", label: "90 days" },
              ]}
            />
          }
        >
          {preview.error ? (
            <p className="text-sm text-red-600">{errorMessage(preview.error)}</p>
          ) : !preview.data ? (
            <Skeleton className="h-24 w-full" />
          ) : (
            <Comparison current={preview.data.current.totals} proposed={preview.data.proposed.totals} days={preview.data.days} />
          )}
        </Card>
      ) : (
        <Notice tone="gray" title="Now">
          {saved.description}
        </Notice>
      )}
    </div>
  );
}

function Comparison({ current, proposed, days }: { current: AttributionTotals; proposed: AttributionTotals; days: number }) {
  const rows: { label: string; key: keyof AttributionTotals; money: boolean }[] = [
    { label: "Revenue from DumbMail", key: "attributed_revenue", money: true },
    { label: "Orders from DumbMail", key: "attributed_conversions", money: false },
    { label: "From campaigns", key: "campaign_revenue", money: true },
    { label: "From flows", key: "flow_revenue", money: true },
    { label: "From email", key: "email_revenue", money: true },
    { label: "From texts", key: "sms_revenue", money: true },
    { label: "From push", key: "push_revenue", money: true },
  ];
  const fmt = (money: boolean, n: number) => (money ? formatCurrency(n) : formatNumber(n));
  return (
    <div>
      <p className="mb-3 text-sm text-zinc-600">
        The last {days} days, counted both ways. Store revenue stays {formatCurrency(proposed.revenue)} from {formatNumber(proposed.conversions)} orders.
      </p>
      <div className="overflow-x-auto">
        <table className="w-full min-w-[420px] text-sm">
          <thead>
            <tr className="text-left text-[11px] font-semibold uppercase tracking-wide text-zinc-500">
              <th className="py-1.5" />
              <th className="py-1.5 text-right">Now</th>
              <th className="py-1.5 text-right">With your changes</th>
            </tr>
          </thead>
          <tbody>
            {rows.map((r) => {
              const a = current[r.key];
              const b = proposed[r.key];
              return (
                <tr key={r.key} className="border-t border-zinc-100">
                  <td className="py-2 text-zinc-700">{r.label}</td>
                  <td className="py-2 text-right tabular-nums text-zinc-600">{fmt(r.money, a)}</td>
                  <td className={`py-2 text-right font-medium tabular-nums ${b > a ? "text-accent-800" : b < a ? "text-amber-800" : "text-zinc-900"}`}>{fmt(r.money, b)}</td>
                </tr>
              );
            })}
          </tbody>
        </table>
      </div>
    </div>
  );
}
