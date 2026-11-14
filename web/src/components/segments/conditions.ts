import type { Condition, EventCountOp, FieldFilter, PropOp, SegmentDefinition } from "../../api";

export const conditionLabels: Record<Condition["type"], string> = {
  property: "Property",
  event: "What someone has done",
  list: "List membership",
  consent: "Consent",
  group: "Group (AND / OR)",
};

export const propOps: { value: PropOp; label: string }[] = [
  { value: "eq", label: "equals" },
  { value: "neq", label: "doesn't equal" },
  { value: "contains", label: "contains" },
  { value: "not_contains", label: "doesn't contain" },
  { value: "starts_with", label: "starts with" },
  { value: "exists", label: "is set" },
  { value: "not_exists", label: "is not set" },
  { value: "gt", label: ">" },
  { value: "gte", label: "≥" },
  { value: "lt", label: "<" },
  { value: "lte", label: "≤" },
  { value: "in", label: "is one of" },
  { value: "within_last_days", label: "is within the last" },
  { value: "more_than_days_ago", label: "is more than … days ago" },
];

export const eventOps: { value: EventCountOp; label: string }[] = [
  { value: "at_least", label: "at least" },
  { value: "at_most", label: "at most" },
  { value: "exactly", label: "exactly" },
  { value: "zero", label: "zero times" },
];

export const numericOps = new Set<PropOp>(["gt", "gte", "lt", "lte", "within_last_days", "more_than_days_ago"]);

export function isValuelessOp(op: PropOp): boolean {
  return op === "exists" || op === "not_exists";
}

export function conditionDefaults(type: Condition["type"], firstListId?: string): Condition {
  switch (type) {
    case "property":
      return { type, field: "", op: "eq", value: "" };
    case "event":
      return { type, metric: "", op: "at_least", count: 1, within_days: 30 };
    case "list":
      return { type, list_id: firstListId ?? "", op: "in" };
    case "consent":
      return { type, channel: "email", status: "subscribed" };
    case "group":
      return { type, match: "any", conditions: [] };
  }
}

export const emptyDefinition = (): SegmentDefinition => ({ match: "all", conditions: [] });

function filterComplete(f: FieldFilter): boolean {
  if (!f.field.trim()) return false;
  if (isValuelessOp(f.op)) return true;
  if (numericOps.has(f.op)) return typeof f.value === "number" && !Number.isNaN(f.value);
  return f.value !== undefined;
}

export function isConditionComplete(c: Condition): boolean {
  switch (c.type) {
    case "property":
      return filterComplete(c);
    case "event":
      return c.metric.trim() !== "" && (c.op === "zero" || typeof c.count === "number") && (c.where ?? []).every(filterComplete);
    case "list":
      return c.list_id !== "";
    case "consent":
      return true;
    case "group":
      return c.conditions.length > 0 && c.conditions.every(isConditionComplete);
  }
}

export function isDefinitionComplete(d: SegmentDefinition): boolean {
  return d.conditions.every(isConditionComplete);
}

function opLabel(op: PropOp) {
  return propOps.find((o) => o.value === op)?.label ?? op;
}

function formatValue(v: unknown): string {
  if (Array.isArray(v)) return v.join(", ");
  if (v === undefined || v === null) return "";
  return String(v);
}

export function filterSummary(f: FieldFilter): string {
  if (isValuelessOp(f.op)) return `${f.field || "?"} ${opLabel(f.op)}`;
  const suffix = f.op === "within_last_days" || f.op === "more_than_days_ago" ? " days" : "";
  return `${f.field || "?"} ${opLabel(f.op)} ${formatValue(f.value)}${suffix}`;
}

/** One-line human summary, used on flow canvas cards. */
export function conditionSummary(c: Condition, listName: (id: string) => string = (id) => id): string {
  switch (c.type) {
    case "property":
      return filterSummary(c);
    case "event": {
      const count = c.op === "zero" ? "zero times" : `${eventOps.find((o) => o.value === c.op)?.label} ${c.count ?? 1}×`;
      const window = c.within_days ? ` in the last ${c.within_days}d` : "";
      return `${c.metric || "?"} ${count}${window}`;
    }
    case "list":
      return `${c.op === "in" ? "in" : "not in"} list ${listName(c.list_id)}`;
    case "consent":
      return `${c.channel} consent is ${c.status.replace("_", " ")}`;
    case "group":
      return `(${c.conditions.map((x) => conditionSummary(x, listName)).join(c.match === "all" ? " AND " : " OR ")})`;
  }
}

export function definitionSummary(d: SegmentDefinition | undefined, listName?: (id: string) => string): string {
  if (!d || d.conditions.length === 0) return "No conditions";
  return d.conditions.map((c) => conditionSummary(c, listName)).join(d.match === "all" ? " AND " : " OR ");
}
