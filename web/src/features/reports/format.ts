import { formatCurrency, formatNumber, formatPercent, formatShortDate } from "../../lib/format";
import type { MeasureKind } from "./types";

export function formatValue(kind: MeasureKind | "group", v: number | undefined): string {
  const n = v ?? 0;
  switch (kind) {
    case "rate":
      return formatPercent(n, n !== 0 && Math.abs(n) < 0.01 ? 2 : 1);
    case "currency":
      return formatCurrency(n, Math.abs(n) < 100);
    case "count":
      return formatNumber(n);
    default:
      return n.toLocaleString("en-US", { maximumFractionDigits: 2 });
  }
}

const monthFmt = new Intl.DateTimeFormat("en-US", { month: "short", year: "numeric", timeZone: "UTC" });

/** Labels for time buckets: days and weeks as "Nov 3", months as "Nov 2026". */
export function formatBucket(group: string, key: string): string {
  if (group === "month" && /^\d{4}-\d{2}-\d{2}$/.test(key)) {
    const [y, m] = key.split("-").map(Number);
    return monthFmt.format(new Date(Date.UTC(y, m - 1, 1)));
  }
  if ((group === "day" || group === "week") && /^\d{4}-\d{2}-\d{2}$/.test(key)) {
    return group === "week" ? `Week of ${formatShortDate(key)}` : formatShortDate(key);
  }
  return key;
}

export const TIME_GROUPS = ["day", "week", "month"];

/** The calendar date `n` days before `date` ("2026-11-17"). */
export function daysBefore(date: string, n: number): string {
  const [y, m, d] = date.split("-").map(Number);
  return new Date(Date.UTC(y, m - 1, d - n)).toISOString().slice(0, 10);
}

export function formatBytes(n: number): string {
  if (n < 1024) return `${n} B`;
  if (n < 1024 * 1024) return `${(n / 1024).toFixed(0)} KB`;
  return `${(n / 1024 / 1024).toFixed(1)} MB`;
}
