import { formatCurrency, formatNumber, formatPercent } from "../../lib/format";
import { Select } from "../../components/ui";
import type { StepResult } from "./types";

export const rangeOptions = [
  { days: 7, label: "Last 7 days" },
  { days: 30, label: "Last 30 days" },
  { days: 90, label: "Last 90 days" },
  { days: 365, label: "Last 12 months" },
];

export function ResultsRange({ days, onChange }: { days: number; onChange: (d: number) => void }) {
  return (
    <Select value={days} onChange={(e) => onChange(Number(e.target.value))} className="w-40" aria-label="Results range">
      {rangeOptions.map((o) => (
        <option key={o.days} value={o.days}>
          {o.label}
        </option>
      ))}
    </Select>
  );
}

/** The numbers under a step on the canvas. "Waiting" opens the list of people there. */
export function StepResultLine({ result, onWaiting }: { result: StepResult; onWaiting?: () => void }) {
  const m = result.messages;
  const n = result.notifications;
  return (
    <div className="mt-2 flex flex-wrap gap-x-3 gap-y-0.5 border-t border-zinc-100 pt-2 text-[11px] tabular-nums text-zinc-600">
      <span>{formatNumber(result.reached)} reached</span>
      {result.waiting > 0 ? (
        onWaiting ? (
          <span
            role="button"
            tabIndex={0}
            onClick={(e) => {
              e.stopPropagation();
              onWaiting();
            }}
            onKeyDown={(e) => {
              if (e.key === "Enter" || e.key === " ") {
                e.preventDefault();
                e.stopPropagation();
                onWaiting();
              }
            }}
            className="font-medium text-accent-700 underline decoration-dotted underline-offset-2 hover:text-accent-900"
          >
            {formatNumber(result.waiting)} waiting
          </span>
        ) : (
          <span>{formatNumber(result.waiting)} waiting</span>
        )
      ) : null}
      {m ? (
        <>
          <span>{formatNumber(m.sent)} sent</span>
          {result.type === "email" ? <span>{formatPercent(m.open_rate)} open</span> : null}
          <span>{formatPercent(m.click_rate)} click</span>
          <span className="font-medium text-zinc-800">{formatCurrency(m.revenue)}</span>
        </>
      ) : null}
      {n ? (
        <>
          <span>{formatNumber(n.sent)} sent</span>
          {n.skipped > 0 ? <span>{formatNumber(n.skipped)} skipped</span> : null}
        </>
      ) : null}
    </div>
  );
}

/** The full numbers for a step, in its side panel. */
export function StepResultDetail({ result }: { result: StepResult }) {
  const m = result.messages;
  const n = result.notifications;
  const cells: [string, string][] = [
    ["Reached", formatNumber(result.reached)],
    ["Waiting now", formatNumber(result.waiting)],
  ];
  if (m) {
    cells.push(
      ["Sent", formatNumber(m.sent)],
      ["Delivered", formatNumber(m.delivered)],
      ...(result.type === "email" ? ([["Opened", `${formatNumber(m.opened)} · ${formatPercent(m.open_rate)}`]] as [string, string][]) : []),
      ["Clicked", `${formatNumber(m.clicked)} · ${formatPercent(m.click_rate)}`],
      ["Bounced", formatNumber(m.bounced)],
      ["Unsubscribed", formatNumber(m.unsubscribed)],
      ["Skipped", formatNumber(m.skipped)],
      ["Revenue", `${formatCurrency(m.revenue)} · ${formatNumber(m.orders)} orders`],
    );
  }
  if (n) cells.push(["Sent", formatNumber(n.sent)], ["Skipped", formatNumber(n.skipped)], ["Failed", formatNumber(n.failed)]);
  return (
    <div className="mb-4 rounded-lg bg-zinc-50 p-3">
      <dl className="grid grid-cols-2 gap-x-4 gap-y-2 text-sm">
        {cells.map(([k, v]) => (
          <div key={k}>
            <dt className="text-[11px] text-zinc-500">{k}</dt>
            <dd className="font-semibold tabular-nums text-zinc-900">{v}</dd>
          </div>
        ))}
      </dl>
      {m && m.skip_reasons.length > 0 ? (
        <div className="mt-3 border-t border-zinc-200 pt-2 text-xs text-zinc-600">
          <div className="mb-1 font-medium text-zinc-700">Why messages were skipped</div>
          <ul className="space-y-0.5">
            {m.skip_reasons.map((r) => (
              <li key={r.reason} className="flex justify-between gap-2">
                <span className="truncate">{r.reason}</span>
                <span className="tabular-nums">{formatNumber(r.count)}</span>
              </li>
            ))}
          </ul>
        </div>
      ) : null}
    </div>
  );
}
