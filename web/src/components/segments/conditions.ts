import type { CmpOp, Condition, EventCountOp, EventWindow, FieldFilter, Prediction, PropOp, SegmentDefinition, TimeOp } from "../../api";

export const conditionLabels: Record<Condition["type"], string> = {
  property: "Property",
  event: "What someone has done",
  event_value: "Total or average of an event",
  event_time: "First or last time they did something",
  engagement: "Engaged recently",
  message: "Got, opened or clicked a message",
  form: "Signed up through a form",
  list: "List membership",
  segment: "In another segment",
  consent: "Consent",
  can_receive: "Can receive email or SMS",
  suppressed: "Email suppressed",
  prediction: "Prediction",
  random: "Random sample",
  group: "Group (AND / OR)",
};

/** Condition types grouped for the pickers. */
export const conditionGroups: { label: string; types: Condition["type"][] }[] = [
  { label: "Profile", types: ["property"] },
  { label: "Activity", types: ["event", "event_value", "event_time", "engagement", "message", "form"] },
  { label: "Membership", types: ["list", "segment"] },
  { label: "Reachability", types: ["consent", "can_receive", "suppressed"] },
  { label: "Predictive", types: ["prediction", "random"] },
  { label: "Logic", types: ["group"] },
];

export const propOps: { value: PropOp; label: string }[] = [
  { value: "eq", label: "equals" },
  { value: "neq", label: "doesn't equal" },
  { value: "contains", label: "contains" },
  { value: "not_contains", label: "doesn't contain" },
  { value: "starts_with", label: "starts with" },
  { value: "ends_with", label: "ends with" },
  { value: "exists", label: "is set" },
  { value: "not_exists", label: "is not set" },
  { value: "gt", label: ">" },
  { value: "gte", label: "≥" },
  { value: "lt", label: "<" },
  { value: "lte", label: "≤" },
  { value: "between", label: "is between" },
  { value: "in", label: "is one of" },
  { value: "not_in", label: "is none of" },
  { value: "within_last_days", label: "is within the last" },
  { value: "more_than_days_ago", label: "is more than … days ago" },
  { value: "anniversary_within_days", label: "anniversary is in the next" },
];

export const eventOps: { value: EventCountOp; label: string }[] = [
  { value: "at_least", label: "at least" },
  { value: "at_most", label: "at most" },
  { value: "exactly", label: "exactly" },
  { value: "zero", label: "zero times" },
];

export const cmpOps: { value: CmpOp; label: string }[] = [
  { value: "gt", label: ">" },
  { value: "gte", label: "≥" },
  { value: "lt", label: "<" },
  { value: "lte", label: "≤" },
  { value: "eq", label: "=" },
  { value: "neq", label: "≠" },
  { value: "between", label: "between" },
];

export const timeOps: { value: TimeOp; label: string }[] = [
  { value: "within_last_days", label: "within the last" },
  { value: "more_than_days_ago", label: "more than … days ago" },
  { value: "after", label: "on or after" },
  { value: "before", label: "before" },
  { value: "between", label: "between" },
];

export const predictions: { value: Prediction; label: string; money?: boolean; days?: boolean }[] = [
  { value: "predicted_clv", label: "Predicted lifetime value", money: true },
  { value: "historic_clv", label: "Revenue so far", money: true },
  { value: "average_order_value", label: "Average order value", money: true },
  { value: "order_count", label: "Number of orders" },
  { value: "days_since_last_order", label: "Days since last order", days: true },
  { value: "average_days_between_orders", label: "Average days between orders", days: true },
  { value: "expected_next_order_in_days", label: "Next order expected in (days)", days: true },
  { value: "churn_risk", label: "Churn risk" },
];

export const numericOps = new Set<PropOp>(["gt", "gte", "lt", "lte", "within_last_days", "more_than_days_ago", "anniversary_within_days"]);

export function isValuelessOp(op: PropOp): boolean {
  return op === "exists" || op === "not_exists";
}

export function conditionDefaults(type: Condition["type"], firstListId?: string): Condition {
  switch (type) {
    case "property":
      return { type, field: "", op: "eq", value: "" };
    case "event":
      return { type, metric: "", op: "at_least", count: 1, within_days: 30 };
    case "event_value":
      return { type, metric: "Placed Order", aggregate: "sum", op: "gte", value: 100 };
    case "event_time":
      return { type, metric: "Placed Order", which: "last", op: "more_than_days_ago", value: 60 };
    case "engagement":
      return { type, channel: "email", engaged: true, within_days: 90 };
    case "message":
      return { type, action: "opened", op: "has" };
    case "form":
      return { type, op: "has" };
    case "list":
      return { type, list_id: firstListId ?? "", op: "in" };
    case "segment":
      return { type, segment_id: "", op: "in" };
    case "consent":
      return { type, channel: "email", status: "subscribed" };
    case "can_receive":
      return { type, channel: "email", value: true };
    case "suppressed":
      return { type, value: true };
    case "prediction":
      return { type, measure: "churn_risk", op: "in", value: ["high"] };
    case "random":
      return { type, percent: 10, seed: "" };
    case "group":
      return { type, match: "any", conditions: [] };
  }
}

export const emptyDefinition = (): SegmentDefinition => ({ match: "all", conditions: [] });

const isNum = (v: unknown): v is number => typeof v === "number" && !Number.isNaN(v);
const isDate = (v: unknown): v is string => typeof v === "string" && /^\d{4}-\d{2}-\d{2}$/.test(v);
const isPair = (v: unknown, ok: (x: unknown) => boolean) => Array.isArray(v) && v.length === 2 && v.every(ok);

function filterComplete(f: FieldFilter): boolean {
  if (!f.field.trim()) return false;
  if (isValuelessOp(f.op)) return true;
  if (numericOps.has(f.op)) return isNum(f.value);
  if (f.op === "between") return isPair(f.value, (x) => x !== "" && x !== undefined && x !== null);
  if (f.op === "in" || f.op === "not_in") return Array.isArray(f.value) && f.value.length > 0;
  return f.value !== undefined;
}

function windowComplete(w: EventWindow): boolean {
  return (w.since === undefined || isDate(w.since)) && (w.until === undefined || isDate(w.until)) && (w.within_days === undefined || w.within_days > 0);
}

export function isConditionComplete(c: Condition): boolean {
  switch (c.type) {
    case "property":
      return filterComplete(c);
    case "event":
      return c.metric.trim() !== "" && (c.op === "zero" || isNum(c.count)) && (c.where ?? []).every(filterComplete) && windowComplete(c);
    case "event_value":
      return c.metric.trim() !== "" && (c.op === "between" ? isPair(c.value, isNum) : isNum(c.value)) && (c.where ?? []).every(filterComplete) && windowComplete(c);
    case "event_time":
      return (
        c.metric.trim() !== "" &&
        (c.op === "between" ? isPair(c.value, isDate) : c.op === "before" || c.op === "after" ? isDate(c.value) : isNum(c.value))
      );
    case "engagement":
      return c.within_days > 0;
    case "message":
      return Boolean(c.campaign_id || c.flow_id);
    case "form":
      return windowComplete(c);
    case "list":
      return c.list_id !== "";
    case "segment":
      return c.segment_id !== "";
    case "consent":
    case "can_receive":
    case "suppressed":
      return true;
    case "prediction":
      if (c.measure === "churn_risk") return c.op === "in" ? Array.isArray(c.value) && c.value.length > 0 : typeof c.value === "string";
      return c.op === "between" ? isPair(c.value, isNum) : isNum(c.value);
    case "random":
      return c.percent > 0 && c.percent <= 100;
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
  if (f.op === "between" && Array.isArray(f.value)) return `${f.field || "?"} is between ${formatValue(f.value[0])} and ${formatValue(f.value[1])}`;
  const suffix = f.op === "within_last_days" || f.op === "more_than_days_ago" || f.op === "anniversary_within_days" ? " days" : "";
  return `${f.field || "?"} ${opLabel(f.op)} ${formatValue(f.value)}${suffix}`;
}

function windowSummary(w: EventWindow): string {
  const parts: string[] = [];
  if (w.within_days) parts.push(`in the last ${w.within_days}d`);
  if (w.since && w.until) parts.push(`from ${w.since} to ${w.until}`);
  else if (w.since) parts.push(`since ${w.since}`);
  else if (w.until) parts.push(`through ${w.until}`);
  return parts.length ? ` ${parts.join(" ")}` : "";
}

function cmpSummary(op: CmpOp | "in", v: unknown): string {
  if (op === "between" && Array.isArray(v)) return `between ${formatValue(v[0])} and ${formatValue(v[1])}`;
  if (op === "in") return `is ${formatValue(v)}`;
  return `${cmpOps.find((o) => o.value === op)?.label ?? op} ${formatValue(v)}`;
}

type Names = { list?: (id: string) => string; segment?: (id: string) => string };

/** One-line human summary, used on flow canvas cards. */
export function conditionSummary(c: Condition, names: Names | ((id: string) => string) = {}): string {
  const n: Names = typeof names === "function" ? { list: names } : names;
  const list = n.list ?? ((id: string) => id);
  const segment = n.segment ?? ((id: string) => id);
  switch (c.type) {
    case "property":
      return filterSummary(c);
    case "event": {
      const count = c.op === "zero" ? "zero times" : `${eventOps.find((o) => o.value === c.op)?.label} ${c.count ?? 1}×`;
      return `${c.metric || "?"} ${count}${windowSummary(c)}`;
    }
    case "event_value":
      return `${c.aggregate} of ${c.metric || "?"} ${c.field && c.field !== "value" ? c.field : "value"} ${cmpSummary(c.op, c.value)}${windowSummary(c)}`;
    case "event_time": {
      const v = Array.isArray(c.value) ? `${c.value[0]} and ${c.value[1]}` : String(c.value);
      const days = c.op === "within_last_days" || c.op === "more_than_days_ago" ? " days" : "";
      return `${c.which} ${c.metric || "?"} ${timeOps.find((o) => o.value === c.op)?.label} ${v}${days}`;
    }
    case "engagement":
      return `${c.engaged ? "engaged" : "not engaged"} (${c.channel}) in the last ${c.within_days}d`;
    case "message":
      return `${c.op === "has" ? "" : "not "}${c.action} ${c.campaign_id ? "campaign" : "flow"} message${c.within_days ? ` in the last ${c.within_days}d` : ""}`;
    case "form":
      return `${c.op === "has" ? "signed up" : "didn't sign up"} through ${c.form_id ? "a chosen form" : "any form"}${windowSummary(c)}`;
    case "list":
      return `${c.op === "in" ? "in" : "not in"} list ${list(c.list_id)}`;
    case "segment":
      return `${c.op === "in" ? "in" : "not in"} segment ${segment(c.segment_id)}`;
    case "consent":
      return `${c.channel} consent is ${c.status.replace("_", " ")}`;
    case "can_receive":
      return `${c.value ? "can" : "can't"} receive ${c.channel === "sms" ? "SMS" : "email"}`;
    case "suppressed":
      return c.value ? "email suppressed" : "email not suppressed";
    case "prediction":
      return `${predictions.find((p) => p.value === c.measure)?.label ?? c.measure} ${cmpSummary(c.op, c.value)}`;
    case "random":
      return `random ${c.percent}%${c.seed ? ` (${c.seed})` : ""}`;
    case "group":
      return `(${c.conditions.map((x) => conditionSummary(x, n)).join(c.match === "all" ? " AND " : " OR ")})`;
  }
}

export function definitionSummary(d: SegmentDefinition | undefined, listName?: (id: string) => string): string {
  if (!d || d.conditions.length === 0) return "No conditions";
  return d.conditions.map((c) => conditionSummary(c, listName)).join(d.match === "all" ? " AND " : " OR ");
}
