import type { FieldFilter } from "../../api/types";
import type { ObjectCondition, ObjectDateTrigger, ObjectTrigger } from "./types";

export function newObjectCondition(): ObjectCondition {
  return { type: "object", object_type: "", op: "at_least", count: 1, where: [] };
}

export function isObjectConditionComplete(c: ObjectCondition, filterComplete: (f: FieldFilter) => boolean): boolean {
  return /^[a-z][a-z0-9_]{0,39}$/.test(c.object_type) && (c.op === "zero" || (typeof c.count === "number" && c.count >= 0)) && (c.where ?? []).every(filterComplete) && (c.relationship === undefined || c.relationship.trim() !== "");
}

export function objectConditionSummary(c: ObjectCondition, filterSummary: (f: FieldFilter) => string): string {
  const what = c.object_type || "?";
  const how = c.op === "zero" ? `has no ${what}` : c.op === "at_least" && (c.count ?? 1) === 1 ? `has a ${what}` : `has ${c.op.replace("_", " ")} ${c.count ?? 1} ${what}`;
  const rel = c.relationship ? ` as ${c.relationship}` : "";
  const where = (c.where ?? []).length ? ` where ${(c.where ?? []).map(filterSummary).join(" and ")}` : "";
  return `${how}${rel}${where}`;
}

export function objectTriggerSummary(t: ObjectTrigger | ObjectDateTrigger): string {
  if (t.type === "object") {
    const what = t.object_type || "an object";
    if (t.field) return `When ${what} ${t.field} changes${t.to !== undefined ? ` to ${String(t.to)}` : ""}`;
    return t.on === "created" ? `When a ${what} is created` : t.on === "updated" ? `When a ${what} is updated` : `When a ${what} is created or updated`;
  }
  const field = `${t.object_type || "object"} ${t.field || "date"}`;
  if (t.offset_hours !== undefined) {
    const n = t.offset_hours;
    return n === 0 ? `At ${field}` : `${Math.abs(n)} ${Math.abs(n) === 1 ? "hour" : "hours"} ${n < 0 ? "before" : "after"} ${field}`;
  }
  const n = t.offset_days ?? 0;
  return n === 0 ? `On ${field}` : `${Math.abs(n)} ${Math.abs(n) === 1 ? "day" : "days"} ${n < 0 ? "before" : "after"} ${field}`;
}
