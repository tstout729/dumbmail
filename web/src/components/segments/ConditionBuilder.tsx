import { useState } from "react";
import { Plus, Trash2, X } from "lucide-react";
import type { Condition, Consent, EventCondition, FieldFilter, PropOp, SegmentDefinition } from "../../api";
import { conditionDefaults, conditionLabels, eventOps, isValuelessOp, numericOps, propOps } from "./conditions";
import { useLists, useMetrics } from "../../api";
import { Button, IconButton, Input, Select, cx } from "../ui";

const standardFields = ["email", "phone_number", "first_name", "last_name", "external_id", "created_at"];

type BuilderProps = {
  value: SegmentDefinition;
  onChange: (value: SegmentDefinition) => void;
  /** Extra property keys to suggest (e.g. seen on profiles). */
  propertySuggestions?: string[];
  emptyHint?: string;
};

/** Edits a SegmentDefinition: top-level match plus condition rows, with nested groups. */
export function ConditionBuilder({ value, onChange, propertySuggestions = [], emptyHint }: BuilderProps) {
  const metrics = useMetrics();
  const lists = useLists();
  const ctx: RowContext = {
    metricNames: (metrics.data ?? []).map((m) => m.name),
    lists: (lists.data ?? []).map((l) => ({ id: l.id, name: l.name })),
    propertyFields: [...standardFields, ...propertySuggestions.filter((p) => !standardFields.includes(p))],
  };
  return (
    <>
      <datalist id="pl-metric-names">
        {ctx.metricNames.map((m) => (
          <option key={m} value={m} />
        ))}
      </datalist>
      <datalist id="pl-property-fields">
        {ctx.propertyFields.map((f) => (
          <option key={f} value={f} />
        ))}
      </datalist>
      <ConditionGroup
        match={value.match}
        conditions={value.conditions}
        onChange={(match, conditions) => onChange({ match, conditions })}
        ctx={ctx}
        depth={0}
        emptyHint={emptyHint}
      />
    </>
  );
}

type RowContext = { metricNames: string[]; lists: { id: string; name: string }[]; propertyFields: string[] };

type GroupProps = {
  match: "all" | "any";
  conditions: Condition[];
  onChange: (match: "all" | "any", conditions: Condition[]) => void;
  ctx: RowContext;
  depth: number;
  emptyHint?: string;
};

function ConditionGroup({ match, conditions, onChange, ctx, depth, emptyHint }: GroupProps) {
  const setAt = (i: number, c: Condition) => onChange(match, conditions.map((x, j) => (j === i ? c : x)));
  const removeAt = (i: number) => onChange(match, conditions.filter((_, j) => j !== i));
  const add = (type: Condition["type"]) => onChange(match, [...conditions, conditionDefaults(type, ctx.lists[0]?.id)]);
  const joiner = match === "all" ? "AND" : "OR";

  return (
    <div className="space-y-2">
      {conditions.length > 1 ? (
        <div className="flex items-center gap-2 text-xs text-zinc-600">
          Match
          <Select
            value={match}
            onChange={(e) => onChange(e.target.value as "all" | "any", conditions)}
            className="h-7 w-auto py-0 text-xs"
            aria-label="Match all or any"
          >
            <option value="all">all of these (AND)</option>
            <option value="any">any of these (OR)</option>
          </Select>
        </div>
      ) : null}
      {conditions.length === 0 ? (
        <div className="rounded-lg border border-dashed border-zinc-300 px-4 py-5 text-center text-sm text-zinc-500">
          {emptyHint ?? "No conditions yet. Add one below."}
        </div>
      ) : null}
      {conditions.map((c, i) => (
        <div key={i}>
          {i > 0 ? (
            <div className="my-1 flex items-center gap-2 pl-3">
              <span className="rounded bg-zinc-100 px-1.5 py-0.5 text-[10px] font-semibold tracking-wider text-zinc-500">{joiner}</span>
            </div>
          ) : null}
          <ConditionRow condition={c} onChange={(n) => setAt(i, n)} onRemove={() => removeAt(i)} ctx={ctx} depth={depth} />
        </div>
      ))}
      <AddConditionMenu onAdd={add} allowGroup={depth < 2} />
    </div>
  );
}

function AddConditionMenu({ onAdd, allowGroup }: { onAdd: (type: Condition["type"]) => void; allowGroup: boolean }) {
  const types: Condition["type"][] = allowGroup ? ["property", "event", "list", "consent", "group"] : ["property", "event", "list", "consent"];
  return (
    <div className="flex flex-wrap items-center gap-1.5 pt-1">
      {types.map((t) => (
        <Button key={t} size="sm" variant="ghost" icon={<Plus className="size-3" />} onClick={() => onAdd(t)} className="border border-dashed border-zinc-300">
          {conditionLabels[t]}
        </Button>
      ))}
    </div>
  );
}

type RowProps = { condition: Condition; onChange: (c: Condition) => void; onRemove: () => void; ctx: RowContext; depth: number };

function ConditionRow({ condition, onChange, onRemove, ctx, depth }: RowProps) {
  const changeType = (type: Condition["type"]) => onChange(conditionDefaults(type, ctx.lists[0]?.id));

  return (
    <div className={cx("rounded-lg border bg-white p-2.5", condition.type === "group" ? "border-accent-200 bg-accent-50/30" : "border-zinc-200")}>
      <div className="flex items-start gap-2">
        <div className="flex min-w-0 flex-1 flex-wrap items-center gap-1.5">
          <Select
            value={condition.type}
            onChange={(e) => changeType(e.target.value as Condition["type"])}
            className="h-8 w-auto text-xs font-medium"
            aria-label="Condition type"
          >
            {(Object.keys(conditionLabels) as Condition["type"][])
              .filter((t) => t !== "group" || depth < 2)
              .map((t) => (
                <option key={t} value={t}>
                  {conditionLabels[t]}
                </option>
              ))}
          </Select>
          <RowBody condition={condition} onChange={onChange} ctx={ctx} depth={depth} />
        </div>
        <IconButton label="Remove condition" onClick={onRemove} size="sm" className="mt-1">
          <Trash2 className="size-3.5" />
        </IconButton>
      </div>
    </div>
  );
}

function RowBody({ condition: c, onChange, ctx, depth }: Omit<RowProps, "onRemove">) {
  switch (c.type) {
    case "property":
      return (
        <FilterFields
          filter={c}
          onChange={(f) => onChange({ ...c, ...f })}
          fieldPlaceholder="property, e.g. city"
          fieldList="pl-property-fields"
        />
      );
    case "event":
      return <EventFields condition={c} onChange={onChange} />;
    case "list":
      return (
        <>
          <Select value={c.op} onChange={(e) => onChange({ ...c, op: e.target.value as "in" | "not_in" })} className="h-8 w-auto text-xs" aria-label="List operator">
            <option value="in">is in</option>
            <option value="not_in">is not in</option>
          </Select>
          <Select value={c.list_id} onChange={(e) => onChange({ ...c, list_id: e.target.value })} className="h-8 w-auto min-w-40 text-xs" aria-label="List">
            {c.list_id === "" ? <option value="">Choose a list…</option> : null}
            {ctx.lists.map((l) => (
              <option key={l.id} value={l.id}>
                {l.name}
              </option>
            ))}
          </Select>
        </>
      );
    case "consent":
      return (
        <>
          <Select value={c.channel} onChange={(e) => onChange({ ...c, channel: e.target.value as "email" | "sms" })} className="h-8 w-auto text-xs" aria-label="Channel">
            <option value="email">Email</option>
            <option value="sms">SMS</option>
          </Select>
          <span className="text-xs text-zinc-500">consent is</span>
          <Select value={c.status} onChange={(e) => onChange({ ...c, status: e.target.value as Consent })} className="h-8 w-auto text-xs" aria-label="Consent status">
            <option value="subscribed">subscribed</option>
            <option value="unsubscribed">unsubscribed</option>
            <option value="never_subscribed">never subscribed</option>
            <option value="pending">waiting to confirm</option>
          </Select>
        </>
      );
    case "group":
      return (
        <div className="mt-1 w-full border-l-2 border-accent-200 pl-3">
          <ConditionGroup
            match={c.match}
            conditions={c.conditions}
            onChange={(match, conditions) => onChange({ ...c, match, conditions })}
            ctx={ctx}
            depth={depth + 1}
            emptyHint="Empty group. Add conditions to combine them."
          />
        </div>
      );
  }
}

function EventFields({ condition: c, onChange }: { condition: EventCondition; onChange: (c: Condition) => void }) {
  const where = c.where ?? [];
  return (
    <>
      <span className="text-xs text-zinc-500">has done</span>
      <Input
        value={c.metric}
        onChange={(e) => onChange({ ...c, metric: e.target.value })}
        list="pl-metric-names"
        placeholder="metric, e.g. Placed Order"
        className="h-8 w-48 text-xs"
        aria-label="Metric"
      />
      <Select
        value={c.op}
        onChange={(e) => {
          const op = e.target.value as EventCondition["op"];
          onChange({ ...c, op, count: op === "zero" ? undefined : (c.count ?? 1) });
        }}
        className="h-8 w-auto text-xs"
        aria-label="Count operator"
      >
        {eventOps.map((o) => (
          <option key={o.value} value={o.value}>
            {o.label}
          </option>
        ))}
      </Select>
      {c.op !== "zero" ? (
        <>
          <Input
            type="number"
            min={0}
            value={c.count ?? ""}
            onChange={(e) => onChange({ ...c, count: e.target.value === "" ? undefined : Number(e.target.value) })}
            className="h-8 w-16 text-xs"
            aria-label="Count"
          />
          <span className="text-xs text-zinc-500">times</span>
        </>
      ) : null}
      <Select
        value={c.within_days === undefined ? "all" : "within"}
        onChange={(e) => onChange({ ...c, within_days: e.target.value === "all" ? undefined : 30 })}
        className="h-8 w-auto text-xs"
        aria-label="Time window"
      >
        <option value="all">over all time</option>
        <option value="within">in the last</option>
      </Select>
      {c.within_days !== undefined ? (
        <>
          <Input
            type="number"
            min={1}
            value={c.within_days}
            onChange={(e) => onChange({ ...c, within_days: Math.max(1, Number(e.target.value) || 1) })}
            className="h-8 w-16 text-xs"
            aria-label="Days"
          />
          <span className="text-xs text-zinc-500">days</span>
        </>
      ) : null}
      <div className="w-full space-y-1.5">
        {where.map((w, i) => (
          <div key={i} className="flex flex-wrap items-center gap-1.5 pl-2">
            <span className="text-xs text-zinc-500">where</span>
            <FilterFields
              filter={w}
              onChange={(f) => onChange({ ...c, where: where.map((x, j) => (j === i ? f : x)) })}
              fieldPlaceholder='"value" or event property'
              fieldList={undefined}
            />
            <IconButton label="Remove filter" size="sm" onClick={() => onChange({ ...c, where: where.filter((_, j) => j !== i) })}>
              <X className="size-3" />
            </IconButton>
          </div>
        ))}
        <button
          type="button"
          onClick={() => onChange({ ...c, where: [...where, { field: "", op: "eq", value: "" }] })}
          className="pl-2 text-xs font-medium text-accent-700 hover:underline"
        >
          + Add event filter
        </button>
      </div>
    </>
  );
}

type FilterFieldsProps = {
  filter: FieldFilter;
  onChange: (f: FieldFilter) => void;
  fieldPlaceholder: string;
  fieldList: string | undefined;
};

/** field + operator + value inputs shared by property conditions, event filters and trigger filters. */
export function FilterFields({ filter, onChange, fieldPlaceholder, fieldList }: FilterFieldsProps) {
  const setOp = (op: PropOp) => {
    let value: unknown = filter.value;
    if (isValuelessOp(op)) value = undefined;
    else if (numericOps.has(op)) value = typeof value === "number" ? value : Number(value) || 0;
    else if (op === "in") value = Array.isArray(value) ? value : value === undefined || value === "" ? [] : [String(value)];
    else value = Array.isArray(value) ? value.join(", ") : typeof value === "number" ? String(value) : (value ?? "");
    onChange({ ...filter, op, value });
  };
  return (
    <>
      <Input
        value={filter.field}
        onChange={(e) => onChange({ ...filter, field: e.target.value })}
        list={fieldList}
        placeholder={fieldPlaceholder}
        className="h-8 w-40 text-xs"
        aria-label="Field"
      />
      <Select value={filter.op} onChange={(e) => setOp(e.target.value as PropOp)} className="h-8 w-auto text-xs" aria-label="Operator">
        {propOps.map((o) => (
          <option key={o.value} value={o.value}>
            {o.label}
          </option>
        ))}
      </Select>
      <ValueInput filter={filter} onChange={(value) => onChange({ ...filter, value })} />
      {filter.op === "within_last_days" || filter.op === "more_than_days_ago" ? <span className="text-xs text-zinc-500">days</span> : null}
    </>
  );
}

function ValueInput({ filter, onChange }: { filter: FieldFilter; onChange: (v: unknown) => void }) {
  if (isValuelessOp(filter.op)) return null;
  if (numericOps.has(filter.op)) {
    return (
      <Input
        type="number"
        value={typeof filter.value === "number" ? filter.value : ""}
        onChange={(e) => onChange(e.target.value === "" ? undefined : Number(e.target.value))}
        className="h-8 w-24 text-xs"
        aria-label="Value"
      />
    );
  }
  if (filter.op === "in") {
    const joined = Array.isArray(filter.value) ? filter.value.map(String).join(", ") : "";
    return <ListValueInput key={joined} initial={joined} onChange={onChange} />;
  }
  return (
    <Input
      value={filter.value === undefined || filter.value === null ? "" : String(filter.value)}
      onChange={(e) => onChange(e.target.value)}
      placeholder="value"
      className="h-8 w-40 text-xs"
      aria-label="Value"
    />
  );
}

/** Comma-separated values; keeps the raw text locally so typing commas and spaces feels natural. */
function ListValueInput({ initial, onChange }: { initial: string; onChange: (v: string[]) => void }) {
  const [text, setText] = useState(initial);
  return (
    <Input
      value={text}
      onChange={(e) => setText(e.target.value)}
      onBlur={() =>
        onChange(
          text
            .split(",")
            .map((s) => s.trim())
            .filter(Boolean),
        )
      }
      placeholder="a, b, c"
      className="h-8 w-48 text-xs"
      aria-label="Values (comma separated)"
    />
  );
}
