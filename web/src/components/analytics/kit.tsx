import type { ReactNode } from "react";
import { ArrowDownRight, ArrowUpRight, Info } from "lucide-react";
import { Skeleton, cx } from "../ui";

export type ChangeKind = "count" | "rate";

export type Change = { text: string; direction: "up" | "down" | "flat" };

/**
 * Change from the previous period. Counts and money change in percent; rates change in percentage
 * points, because "open rate up 10%" next to "48.0%" reads as ambiguous.
 */
export function change(current: number, previous: number, kind: ChangeKind, digits = 1): Change | null {
  if (kind === "rate") {
    const diff = (current - previous) * 100;
    if (current === 0 && previous === 0) return null;
    if (Math.abs(diff) < 0.5 * 10 ** -digits) return { text: "No change", direction: "flat" };
    return { text: `${diff > 0 ? "+" : "−"}${Math.abs(diff).toFixed(digits)} pts`, direction: diff > 0 ? "up" : "down" };
  }
  if (previous === 0) return current === 0 ? null : { text: "New", direction: "up" };
  const pct = (current - previous) / previous;
  if (Math.abs(pct) < 0.005) return { text: "No change", direction: "flat" };
  const shown = Math.abs(pct) >= 10 ? `${Math.round(Math.abs(pct))}×` : `${Math.round(Math.abs(pct) * 100)}%`;
  return { text: `${pct > 0 ? "+" : "−"}${shown}`, direction: pct > 0 ? "up" : "down" };
}

/** A signed change, green when it moved the good way and red when it didn't. */
export function DeltaBadge({ value, upIsGood = true, label }: { value: Change | null; upIsGood?: boolean; label?: string }) {
  if (!value) return null;
  const good = value.direction === "flat" ? null : (value.direction === "up") === upIsGood;
  const Icon = value.direction === "up" ? ArrowUpRight : value.direction === "down" ? ArrowDownRight : null;
  return (
    <span
      className={cx(
        "inline-flex items-center gap-0.5 rounded-full px-1.5 py-px text-[11px] font-medium whitespace-nowrap",
        good === null ? "bg-zinc-100 text-zinc-600" : good ? "bg-accent-50 text-accent-800" : "bg-red-50 text-red-700",
      )}
      title={label}
    >
      {Icon ? <Icon className="size-3" aria-hidden /> : null}
      {value.text}
      {label ? <span className="sr-only"> {label}</span> : null}
    </span>
  );
}

/** A tiny trend line. Decorative: the tile's value and change carry the meaning. */
export function Sparkline({ values, className, color = "#219464" }: { values: number[]; className?: string; color?: string }) {
  if (values.length < 2) return null;
  const w = 100;
  const h = 28;
  const max = Math.max(...values);
  const min = Math.min(0, ...values);
  const span = max - min || 1;
  const pts = values.map((v, i) => [(i / (values.length - 1)) * w, h - 2 - ((v - min) / span) * (h - 4)] as const);
  const line = pts.map(([x, y], i) => `${i ? "L" : "M"}${x.toFixed(2)},${y.toFixed(2)}`).join("");
  return (
    <svg viewBox={`0 0 ${w} ${h}`} preserveAspectRatio="none" className={cx("h-7 w-full overflow-visible", className)} aria-hidden>
      <path d={`${line}L${w},${h}L0,${h}Z`} fill={color} opacity={0.08} />
      <path d={line} fill="none" stroke={color} strokeWidth={1.5} vectorEffect="non-scaling-stroke" strokeLinejoin="round" strokeLinecap="round" />
    </svg>
  );
}

type KpiTileProps = {
  label: string;
  value: ReactNode;
  change?: Change | null;
  upIsGood?: boolean;
  compareLabel?: string;
  sub?: ReactNode;
  trend?: number[];
  hint?: string;
  loading?: boolean;
};

export function KpiTile({ label, value, change: delta, upIsGood, compareLabel, sub, trend, hint, loading }: KpiTileProps) {
  return (
    <div className="flex min-w-0 flex-col rounded-lg border border-zinc-200 bg-white px-4 pt-3 pb-2 shadow-xs">
      <div className="flex items-center gap-1 text-xs font-medium text-zinc-500">
        {label}
        {hint ? <Hint text={hint} /> : null}
      </div>
      {loading ? (
        <Skeleton className="mt-2 h-7 w-24" />
      ) : (
        <div className="mt-1 flex flex-wrap items-baseline gap-x-2 gap-y-1">
          <span className="text-2xl font-semibold tracking-tight text-zinc-950">{value}</span>
          <DeltaBadge value={delta ?? null} upIsGood={upIsGood} label={compareLabel} />
        </div>
      )}
      {sub ? <div className="mt-0.5 truncate text-xs text-zinc-500">{sub}</div> : null}
      <div className="mt-auto pt-2">{trend && !loading ? <Sparkline values={trend} /> : <div className="h-7" />}</div>
    </div>
  );
}

/** A small "i" that explains a number on hover or focus. */
export function Hint({ text }: { text: string }) {
  return (
    <span className="group relative inline-flex">
      <button type="button" className="rounded-full text-zinc-400 hover:text-zinc-700" aria-label={text}>
        <Info className="size-3.5" />
      </button>
      <span
        role="tooltip"
        className="pointer-events-none absolute top-full left-1/2 z-20 mt-1.5 hidden w-60 -translate-x-1/2 rounded-md bg-zinc-900 px-2.5 py-1.5 text-xs leading-snug font-normal text-white shadow-lg group-focus-within:block group-hover:block"
      >
        {text}
      </span>
    </span>
  );
}

/** One whole split into parts: a thin rounded bar, with names and amounts listed beside it. */
export function SplitBar({ parts, className }: { parts: { key: string; value: number; color: string; label: string }[]; className?: string }) {
  const total = parts.reduce((s, p) => s + p.value, 0);
  return (
    <div
      className={cx("flex h-2 w-full gap-0.5 overflow-hidden rounded-full bg-zinc-100", className)}
      role="img"
      aria-label={parts.map((p) => `${p.label} ${total ? Math.round((p.value / total) * 100) : 0}%`).join(", ")}
    >
      {total > 0
        ? parts
            .filter((p) => p.value > 0)
            .map((p) => <div key={p.key} className="h-full first:rounded-l-full last:rounded-r-full" style={{ width: `${(p.value / total) * 100}%`, background: p.color }} />)
        : null}
    </div>
  );
}

export function LegendDot({ color, children }: { color: string; children: ReactNode }) {
  return (
    <span className="inline-flex items-center gap-1.5 text-xs text-zinc-600">
      <span className="size-2 shrink-0 rounded-full" style={{ background: color }} />
      {children}
    </span>
  );
}
