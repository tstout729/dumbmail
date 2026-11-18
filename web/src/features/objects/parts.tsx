import type { FieldDef, FieldKind, FieldValues } from "./types";

export const kindLabels: Record<FieldKind, string> = {
  text: "Text",
  number: "Number",
  money: "Money",
  boolean: "Yes / no",
  date: "Date",
  datetime: "Date and time",
  enum: "One of a list",
  list: "List of text",
};

export const fieldLabel = (f: FieldDef) => f.label?.trim() || f.key;

const plainDate = (s: string) => /^\d{4}-\d{2}-\d{2}$/.test(s);

/** A field's value as people read it: dates in words, date-times in the account's time zone. */
export function formatValue(def: FieldDef | undefined, v: unknown, timeZone: string): string {
  if (v === undefined || v === null || v === "") return "—";
  if (Array.isArray(v)) return v.map(String).join(", ");
  if (typeof v === "boolean") return v ? "Yes" : "No";
  if (typeof v === "string" && (def?.type === "date" || (!def && plainDate(v))) && plainDate(v)) {
    const [y, m, d] = v.split("-").map(Number);
    return new Intl.DateTimeFormat("en-US", { timeZone: "UTC", month: "short", day: "numeric", year: "numeric" }).format(new Date(Date.UTC(y, m - 1, d)));
  }
  if (typeof v === "string" && def?.type === "datetime" && !Number.isNaN(Date.parse(v))) {
    return new Intl.DateTimeFormat("en-US", { timeZone, month: "short", day: "numeric", year: "numeric", hour: "numeric", minute: "2-digit" }).format(new Date(v));
  }
  if (typeof v === "number" && def?.type === "money") {
    try {
      return new Intl.NumberFormat("en-US", { style: "currency", currency: def.currency ?? "USD" }).format(v);
    } catch {
      return String(v);
    }
  }
  return typeof v === "object" ? JSON.stringify(v) : String(v);
}

/** The fields worth a column in a list: dates and choices first, never sensitive ones. */
export function keyFields(fields: FieldDef[], titleField: string | null, max = 3): FieldDef[] {
  const visible = fields.filter((f) => !f.sensitive && f.key !== titleField);
  const rank = (f: FieldDef) => (f.type === "date" || f.type === "datetime" ? 0 : f.type === "enum" ? 1 : 2);
  return [...visible].sort((a, b) => rank(a) - rank(b)).slice(0, max);
}

/** Field definition list (label: value) for one object. */
export function FieldList({ fields, values, timeZone }: { fields: FieldDef[]; values: FieldValues; timeZone: string }) {
  const known = new Set(fields.map((f) => f.key));
  const extra = Object.keys(values).filter((k) => !known.has(k));
  const rows: { key: string; label: string; value: string; def?: FieldDef }[] = [
    ...fields.filter((f) => f.key in values).map((f) => ({ key: f.key, label: fieldLabel(f), value: formatValue(f, values[f.key], timeZone), def: f })),
    ...extra.map((k) => ({ key: k, label: k, value: formatValue(undefined, values[k], timeZone) })),
  ];
  if (rows.length === 0) return <p className="text-sm text-zinc-500">No fields yet.</p>;
  return (
    <dl className="grid grid-cols-1 gap-x-6 gap-y-3 sm:grid-cols-2">
      {rows.map((r) => (
        <div key={r.key} className="min-w-0">
          <dt className="text-xs text-zinc-500">{r.label}</dt>
          <dd className="truncate text-sm text-zinc-900" title={r.value}>
            {r.value}
          </dd>
        </div>
      ))}
    </dl>
  );
}
