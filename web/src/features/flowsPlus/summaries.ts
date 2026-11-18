import type { DelayNode } from "../../api/types";
import type { NotifyNode, Weekday } from "./types";

export const weekdays: { value: Weekday; label: string }[] = [
  { value: "mon", label: "Mon" },
  { value: "tue", label: "Tue" },
  { value: "wed", label: "Wed" },
  { value: "thu", label: "Thu" },
  { value: "fri", label: "Fri" },
  { value: "sat", label: "Sat" },
  { value: "sun", label: "Sun" },
];

function plural(n: number, unit: string): string {
  return `${n} ${n === 1 ? unit.replace(/s$/, "") : unit}`;
}

function daysText(days: Weekday[]): string {
  const order = weekdays.map((d) => d.value);
  const idx = days.map((d) => order.indexOf(d)).sort((a, b) => a - b);
  const contiguous = idx.length > 2 && idx.every((v, i) => i === 0 || v === idx[i - 1] + 1);
  const label = (i: number) => weekdays[i]?.label ?? "";
  if (contiguous) return `${label(idx[0])}–${label(idx[idx.length - 1])}`;
  return idx.map(label).join(", ");
}

/** One line for a delay step on the canvas. */
export function delaySummary(node: DelayNode): string {
  const parts: string[] = [];
  if (node.until) {
    const u = node.until;
    const amount = u.offset_amount ?? 0;
    const field = u.field || "a date";
    parts.push(amount > 0 ? `Until ${plural(amount, u.offset_unit ?? "days")} ${u.offset_direction ?? "after"} ${field}` : `Until ${field}`);
  } else {
    parts.push(`Wait ${plural(node.amount, node.unit)}`);
  }
  if (node.at) parts.push(`at ${node.at}`);
  if (node.weekdays?.length) parts.push(`on ${daysText(node.weekdays)}`);
  return parts.join(", ");
}

export function notifySummary(node: NotifyNode): { text: string; warning?: string } {
  const who = [...(node.to ?? []), ...(node.to_property ? [`profile ${node.to_property}`] : [])];
  if (who.length === 0) return { text: "No recipients", warning: "Add who to notify" };
  if (!node.subject.trim() || !node.body.trim()) return { text: who.join(", "), warning: "Write a subject and message" };
  return { text: `Email ${who.join(", ")}` };
}

/** Whether a node reads the event that started the run (for the untrusted-event note). */
export function readsEvent(node: { type: string; until?: { field: string } | null; event_condition?: { filters: unknown[] } }): boolean {
  if (node.type === "delay") return !!node.until?.field.startsWith("event.");
  if (node.type === "condition") return (node.event_condition?.filters.length ?? 0) > 0;
  return false;
}

const opText: Record<string, string> = { eq: "=", neq: "≠", gt: ">", gte: "≥", lt: "<", lte: "≤", contains: "contains", exists: "is set", not_exists: "is not set" };

/** The event filters of a trigger split, in one line ("" when it has none). */
export function eventSplitSummary(split: { match: "all" | "any"; filters: { field: string; op: string; value?: unknown }[] } | undefined): string {
  if (!split?.filters.length) return "";
  const parts = split.filters.map((f) => {
    const value = f.op === "exists" || f.op === "not_exists" ? "" : ` ${Array.isArray(f.value) ? f.value.join(", ") : String(f.value ?? "")}`;
    return `${f.field} ${opText[f.op] ?? f.op.replace(/_/g, " ")}${value}`;
  });
  return `Event: ${parts.join(split.match === "any" ? " or " : " and ")}`;
}
