import { useState } from "react";
import { Trash2, X } from "lucide-react";
import type {
  CmpOp,
  Condition,
  Consent,
  EventCondition,
  EventValueCondition,
  EventTimeCondition,
  EventWindow,
  FieldFilter,
  PredictionCondition,
  PropOp,
  SegmentDefinition,
  TimeOp,
} from "../../api";
import { conditionDefaults, conditionGroups, conditionLabels, cmpOps, eventOps, isValuelessOp, numericOps, predictions, propOps, timeOps } from "./conditions";
import { useCampaigns, useFlows, useForms, useLists, useMetrics, useSegments } from "../../api";
import { IconButton, Input, Select, cx } from "../ui";

const standardFields = [
  "email",
  "phone_number",
  "first_name",
  "last_name",
  "external_id",
  "created_at",
  "updated_at",
  "last_event_at",
  "email_consent",
  "sms_consent",
  "email_suppressed_reason",
  "country",
  "region",
  "city",
  "zip",
  "timezone",
  "birthday",
];

type Option = { id: string; name: string };

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
  const segments = useSegments();
  const campaigns = useCampaigns();
  const flows = useFlows();
  const forms = useForms();
  const ctx: RowContext = {
    metricNames: (metrics.data ?? []).map((m) => m.name),
    lists: (lists.data ?? []).map((l) => ({ id: l.id, name: l.name })),
    segments: (segments.data ?? []).map((s) => ({ id: s.id, name: s.name })),
    campaigns: (campaigns.data ?? []).map((c) => ({ id: c.id, name: c.name })),
    flows: (flows.data ?? []).map((f) => ({ id: f.id, name: f.name })),
    forms: (forms.data ?? []).map((f) => ({ id: f.id, name: f.name })),
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

type RowContext = {
  metricNames: string[];
  lists: Option[];
  segments: Option[];
  campaigns: Option[];
  flows: Option[];
  forms: Option[];
  propertyFields: string[];
};

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
      <div className="pt-1">
        <Select
          value=""
          onChange={(e) => {
            if (e.target.value) add(e.target.value as Condition["type"]);
          }}
          className="h-8 w-auto border-dashed text-xs font-medium text-accent-700"
          aria-label="Add condition"
        >
          <option value="">+ Add condition…</option>
          <TypeOptions allowGroup={depth < 2} />
        </Select>
      </div>
    </div>
  );
}

/** Condition types as grouped <option>s. */
function TypeOptions({ allowGroup }: { allowGroup: boolean }) {
  return (
    <>
      {conditionGroups.map((g) => {
        const types = g.types.filter((t) => t !== "group" || allowGroup);
        if (types.length === 0) return null;
        return (
          <optgroup key={g.label} label={g.label}>
            {types.map((t) => (
              <option key={t} value={t}>
                {conditionLabels[t]}
              </option>
            ))}
          </optgroup>
        );
      })}
    </>
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
            <TypeOptions allowGroup={depth < 2} />
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

const small = "h-8 w-auto text-xs";
const Hint = ({ children }: { children: React.ReactNode }) => <span className="text-xs text-zinc-500">{children}</span>;

function NumInput({ value, onChange, label, className = "w-20", min, step }: { value: unknown; onChange: (v: number | undefined) => void; label: string; className?: string; min?: number; step?: number }) {
  return (
    <Input
      type="number"
      min={min}
      step={step ?? "any"}
      value={typeof value === "number" && !Number.isNaN(value) ? value : ""}
      onChange={(e) => onChange(e.target.value === "" ? undefined : Number(e.target.value))}
      className={cx("h-8 text-xs", className)}
      aria-label={label}
    />
  );
}

function DateInput({ value, onChange, label }: { value: unknown; onChange: (v: string | undefined) => void; label: string }) {
  return <Input type="date" value={typeof value === "string" ? value : ""} onChange={(e) => onChange(e.target.value || undefined)} className="h-8 w-36 text-xs" aria-label={label} />;
}

function Picker({ value, options, onChange, placeholder, label, allowEmpty }: { value: string | undefined; options: Option[]; onChange: (id: string | undefined) => void; placeholder: string; label: string; allowEmpty?: boolean }) {
  return (
    <Select value={value ?? ""} onChange={(e) => onChange(e.target.value || undefined)} className={cx(small, "min-w-40")} aria-label={label}>
      {allowEmpty || !value ? <option value="">{placeholder}</option> : null}
      {options.map((o) => (
        <option key={o.id} value={o.id}>
          {o.name}
        </option>
      ))}
    </Select>
  );
}

function MetricInput({ value, onChange }: { value: string; onChange: (v: string) => void }) {
  return <Input value={value} onChange={(e) => onChange(e.target.value)} list="pl-metric-names" placeholder="metric, e.g. Placed Order" className="h-8 w-44 text-xs" aria-label="Metric" />;
}

function RowBody({ condition: c, onChange, ctx, depth }: Omit<RowProps, "onRemove">) {
  switch (c.type) {
    case "property":
      return <FilterFields filter={c} onChange={(f) => onChange({ ...c, ...f })} fieldPlaceholder="property, e.g. city" fieldList="pl-property-fields" />;
    case "event":
      return <EventFields condition={c} onChange={onChange} />;
    case "event_value":
      return <EventValueFields condition={c} onChange={onChange} />;
    case "event_time":
      return <EventTimeFields condition={c} onChange={onChange} />;
    case "engagement":
      return (
        <>
          <Select value={c.engaged ? "yes" : "no"} onChange={(e) => onChange({ ...c, engaged: e.target.value === "yes" })} className={small} aria-label="Engaged or not">
            <option value="yes">engaged</option>
            <option value="no">not engaged</option>
          </Select>
          <Hint>with</Hint>
          <Select value={c.channel} onChange={(e) => onChange({ ...c, channel: e.target.value as typeof c.channel })} className={small} aria-label="Channel">
            <option value="email">email (opened or clicked)</option>
            <option value="sms">SMS (clicked or replied)</option>
            <option value="any">anything (email, SMS, site, orders)</option>
          </Select>
          <Hint>in the last</Hint>
          <NumInput value={c.within_days} onChange={(v) => onChange({ ...c, within_days: Math.max(1, v ?? 1) })} label="Days" className="w-16" min={1} step={1} />
          <Hint>days</Hint>
        </>
      );
    case "message": {
      const kind = c.flow_id !== undefined ? "flow" : "campaign";
      return (
        <>
          <Select value={c.op} onChange={(e) => onChange({ ...c, op: e.target.value as typeof c.op })} className={small} aria-label="Has or has not">
            <option value="has">has</option>
            <option value="has_not">has not</option>
          </Select>
          <Select value={c.action} onChange={(e) => onChange({ ...c, action: e.target.value as typeof c.action })} className={small} aria-label="Action">
            <option value="received">received</option>
            <option value="opened">opened</option>
            <option value="clicked">clicked</option>
          </Select>
          <Hint>a message from</Hint>
          <Select
            value={kind}
            onChange={(e) => onChange(e.target.value === "flow" ? { ...c, campaign_id: undefined, flow_id: "" } : { ...c, flow_id: undefined, campaign_id: undefined })}
            className={small}
            aria-label="Campaign or flow"
          >
            <option value="campaign">campaign</option>
            <option value="flow">flow</option>
          </Select>
          {kind === "campaign" ? (
            <Picker value={c.campaign_id} options={ctx.campaigns} onChange={(id) => onChange({ ...c, campaign_id: id })} placeholder="Choose a campaign…" label="Campaign" />
          ) : (
            <Picker value={c.flow_id || undefined} options={ctx.flows} onChange={(id) => onChange({ ...c, flow_id: id ?? "" })} placeholder="Choose a flow…" label="Flow" />
          )}
          <OptionalDays value={c.within_days} onChange={(within_days) => onChange({ ...c, within_days })} />
        </>
      );
    }
    case "form":
      return (
        <>
          <Select value={c.op} onChange={(e) => onChange({ ...c, op: e.target.value as typeof c.op })} className={small} aria-label="Has or has not">
            <option value="has">signed up</option>
            <option value="has_not">didn't sign up</option>
          </Select>
          <Hint>through</Hint>
          <Picker value={c.form_id} options={ctx.forms} onChange={(form_id) => onChange({ ...c, form_id })} placeholder="any form" label="Form" allowEmpty />
          <WindowFields value={c} onChange={(w) => onChange({ ...c, ...w })} />
        </>
      );
    case "list":
      return (
        <>
          <Select value={c.op} onChange={(e) => onChange({ ...c, op: e.target.value as "in" | "not_in" })} className={small} aria-label="List operator">
            <option value="in">is in</option>
            <option value="not_in">is not in</option>
          </Select>
          <Picker value={c.list_id || undefined} options={ctx.lists} onChange={(id) => onChange({ ...c, list_id: id ?? "" })} placeholder="Choose a list…" label="List" />
        </>
      );
    case "segment":
      return (
        <>
          <Select value={c.op} onChange={(e) => onChange({ ...c, op: e.target.value as "in" | "not_in" })} className={small} aria-label="Segment operator">
            <option value="in">is in</option>
            <option value="not_in">is not in</option>
          </Select>
          <Picker value={c.segment_id || undefined} options={ctx.segments} onChange={(id) => onChange({ ...c, segment_id: id ?? "" })} placeholder="Choose a segment…" label="Segment" />
          <Hint>(its current rules, live)</Hint>
        </>
      );
    case "consent":
      return (
        <>
          <Select value={c.channel} onChange={(e) => onChange({ ...c, channel: e.target.value as "email" | "sms" })} className={small} aria-label="Channel">
            <option value="email">Email</option>
            <option value="sms">SMS</option>
          </Select>
          <Hint>consent is</Hint>
          <Select value={c.status} onChange={(e) => onChange({ ...c, status: e.target.value as Consent })} className={small} aria-label="Consent status">
            <option value="subscribed">subscribed</option>
            <option value="unsubscribed">unsubscribed</option>
            <option value="never_subscribed">never subscribed</option>
            <option value="pending">waiting to confirm</option>
          </Select>
        </>
      );
    case "can_receive":
      return (
        <>
          <Select value={c.value ? "yes" : "no"} onChange={(e) => onChange({ ...c, value: e.target.value === "yes" })} className={small} aria-label="Can or can't">
            <option value="yes">can</option>
            <option value="no">can't</option>
          </Select>
          <Hint>receive</Hint>
          <Select value={c.channel} onChange={(e) => onChange({ ...c, channel: e.target.value as "email" | "sms" })} className={small} aria-label="Channel">
            <option value="email">email</option>
            <option value="sms">SMS</option>
          </Select>
          <Hint>right now (address, consent and no suppression)</Hint>
        </>
      );
    case "suppressed":
      return (
        <>
          <Hint>email</Hint>
          <Select value={c.value ? "yes" : "no"} onChange={(e) => onChange({ ...c, value: e.target.value === "yes" })} className={small} aria-label="Suppressed or not">
            <option value="yes">is suppressed</option>
            <option value="no">is not suppressed</option>
          </Select>
          <Hint>(hard bounce or spam complaint)</Hint>
        </>
      );
    case "prediction":
      return <PredictionFields condition={c} onChange={onChange} />;
    case "random":
      return (
        <>
          <Hint>keep a random</Hint>
          <NumInput value={c.percent} onChange={(v) => onChange({ ...c, percent: v ?? 0 })} label="Percent" className="w-16" min={0.01} />
          <Hint>% of people, seed</Hint>
          <Input value={c.seed ?? ""} onChange={(e) => onChange({ ...c, seed: e.target.value })} placeholder="e.g. holdout-nov" className="h-8 w-36 text-xs" aria-label="Seed" />
          <Hint>(the same seed picks the same people)</Hint>
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

function OptionalDays({ value, onChange }: { value: number | undefined; onChange: (v: number | undefined) => void }) {
  return (
    <>
      <Select value={value === undefined ? "all" : "within"} onChange={(e) => onChange(e.target.value === "all" ? undefined : 30)} className={small} aria-label="Time window">
        <option value="all">ever</option>
        <option value="within">in the last</option>
      </Select>
      {value !== undefined ? (
        <>
          <NumInput value={value} onChange={(v) => onChange(Math.max(1, v ?? 1))} label="Days" className="w-16" min={1} step={1} />
          <Hint>days</Hint>
        </>
      ) : null}
    </>
  );
}

/** Over all time, in the last N days, or between two dates (account time zone). */
function WindowFields({ value, onChange }: { value: EventWindow; onChange: (w: EventWindow) => void }) {
  const mode = value.since !== undefined || value.until !== undefined ? "dates" : value.within_days !== undefined ? "within" : "all";
  return (
    <>
      <Select
        value={mode}
        onChange={(e) => {
          const m = e.target.value;
          if (m === "all") onChange({ within_days: undefined, since: undefined, until: undefined });
          else if (m === "within") onChange({ within_days: 30, since: undefined, until: undefined });
          else onChange({ within_days: undefined, since: "", until: "" });
        }}
        className={small}
        aria-label="Time window"
      >
        <option value="all">over all time</option>
        <option value="within">in the last</option>
        <option value="dates">between dates</option>
      </Select>
      {mode === "within" ? (
        <>
          <NumInput value={value.within_days} onChange={(v) => onChange({ ...value, within_days: Math.max(1, v ?? 1) })} label="Days" className="w-16" min={1} step={1} />
          <Hint>days</Hint>
        </>
      ) : null}
      {mode === "dates" ? (
        <>
          <DateInput value={value.since} onChange={(since) => onChange({ ...value, since: since ?? "" })} label="From" />
          <Hint>to</Hint>
          <DateInput value={value.until} onChange={(until) => onChange({ ...value, until: until ?? "" })} label="To" />
        </>
      ) : null}
    </>
  );
}

function WhereFilters({ where, onChange }: { where: FieldFilter[]; onChange: (w: FieldFilter[]) => void }) {
  return (
    <div className="w-full space-y-1.5">
      {where.map((w, i) => (
        <div key={i} className="flex flex-wrap items-center gap-1.5 pl-2">
          <Hint>where</Hint>
          <FilterFields filter={w} onChange={(f) => onChange(where.map((x, j) => (j === i ? f : x)))} fieldPlaceholder='"value" or event property' fieldList={undefined} />
          <IconButton label="Remove filter" size="sm" onClick={() => onChange(where.filter((_, j) => j !== i))}>
            <X className="size-3" />
          </IconButton>
        </div>
      ))}
      <button type="button" onClick={() => onChange([...where, { field: "", op: "eq", value: "" }])} className="pl-2 text-xs font-medium text-accent-700 hover:underline">
        + Add event filter
      </button>
    </div>
  );
}

function EventFields({ condition: c, onChange }: { condition: EventCondition; onChange: (c: Condition) => void }) {
  return (
    <>
      <Hint>has done</Hint>
      <MetricInput value={c.metric} onChange={(metric) => onChange({ ...c, metric })} />
      <Select
        value={c.op}
        onChange={(e) => {
          const op = e.target.value as EventCondition["op"];
          onChange({ ...c, op, count: op === "zero" ? undefined : (c.count ?? 1) });
        }}
        className={small}
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
          <NumInput value={c.count} onChange={(count) => onChange({ ...c, count })} label="Count" className="w-16" min={0} step={1} />
          <Hint>times</Hint>
        </>
      ) : null}
      <WindowFields value={c} onChange={(w) => onChange({ ...c, ...w })} />
      <WhereFilters where={c.where ?? []} onChange={(where) => onChange({ ...c, where })} />
    </>
  );
}

function CmpValue({ op, value, onChange, prefix }: { op: CmpOp; value: unknown; onChange: (v: number | [number, number]) => void; prefix?: string }) {
  if (op === "between") {
    const [lo, hi] = Array.isArray(value) ? value : [undefined, undefined];
    return (
      <>
        {prefix ? <Hint>{prefix}</Hint> : null}
        <NumInput value={lo} onChange={(v) => onChange([v ?? 0, typeof hi === "number" ? hi : 0])} label="From" />
        <Hint>and</Hint>
        {prefix ? <Hint>{prefix}</Hint> : null}
        <NumInput value={hi} onChange={(v) => onChange([typeof lo === "number" ? lo : 0, v ?? 0])} label="To" />
      </>
    );
  }
  return (
    <>
      {prefix ? <Hint>{prefix}</Hint> : null}
      <NumInput value={value} onChange={(v) => onChange(v ?? 0)} label="Value" className="w-24" />
    </>
  );
}

function CmpSelect({ value, onChange }: { value: CmpOp; onChange: (op: CmpOp) => void }) {
  return (
    <Select value={value} onChange={(e) => onChange(e.target.value as CmpOp)} className={small} aria-label="Comparison">
      {cmpOps.map((o) => (
        <option key={o.value} value={o.value}>
          {o.label}
        </option>
      ))}
    </Select>
  );
}

const reshape = (op: CmpOp, v: unknown): number | [number, number] => {
  if (op === "between") return Array.isArray(v) ? (v as [number, number]) : [typeof v === "number" ? v : 0, typeof v === "number" ? v : 0];
  return Array.isArray(v) ? Number(v[0]) || 0 : typeof v === "number" ? v : 0;
};

function EventValueFields({ condition: c, onChange }: { condition: EventValueCondition; onChange: (c: Condition) => void }) {
  return (
    <>
      <Select value={c.aggregate} onChange={(e) => onChange({ ...c, aggregate: e.target.value as EventValueCondition["aggregate"] })} className={small} aria-label="Aggregate">
        <option value="sum">total</option>
        <option value="average">average</option>
        <option value="min">smallest</option>
        <option value="max">largest</option>
      </Select>
      <Input
        value={c.field ?? ""}
        onChange={(e) => onChange({ ...c, field: e.target.value || undefined })}
        placeholder="value (revenue)"
        className="h-8 w-32 text-xs"
        aria-label="Field (blank for the event's value)"
      />
      <Hint>of</Hint>
      <MetricInput value={c.metric} onChange={(metric) => onChange({ ...c, metric })} />
      <Hint>is</Hint>
      <CmpSelect value={c.op} onChange={(op) => onChange({ ...c, op, value: reshape(op, c.value) })} />
      <CmpValue op={c.op} value={c.value} onChange={(value) => onChange({ ...c, value })} />
      <WindowFields value={c} onChange={(w) => onChange({ ...c, ...w })} />
      <WhereFilters where={c.where ?? []} onChange={(where) => onChange({ ...c, where })} />
    </>
  );
}

function EventTimeFields({ condition: c, onChange }: { condition: EventTimeCondition; onChange: (c: Condition) => void }) {
  const setOp = (op: TimeOp) => {
    const value: EventTimeCondition["value"] = op === "between" ? ["", ""] : op === "before" || op === "after" ? "" : 30;
    onChange({ ...c, op, value });
  };
  return (
    <>
      <Hint>the</Hint>
      <Select value={c.which} onChange={(e) => onChange({ ...c, which: e.target.value as "first" | "last" })} className={small} aria-label="First or last">
        <option value="last">last</option>
        <option value="first">first</option>
      </Select>
      <MetricInput value={c.metric} onChange={(metric) => onChange({ ...c, metric })} />
      <Hint>was</Hint>
      <Select value={c.op} onChange={(e) => setOp(e.target.value as TimeOp)} className={small} aria-label="When">
        {timeOps.map((o) => (
          <option key={o.value} value={o.value}>
            {o.label}
          </option>
        ))}
      </Select>
      {c.op === "within_last_days" || c.op === "more_than_days_ago" ? (
        <>
          <NumInput value={c.value} onChange={(v) => onChange({ ...c, value: v ?? 0 })} label="Days" className="w-16" min={0} />
          <Hint>days{c.op === "more_than_days_ago" ? " ago" : ""}</Hint>
        </>
      ) : c.op === "between" ? (
        <>
          <DateInput value={Array.isArray(c.value) ? c.value[0] : ""} onChange={(v) => onChange({ ...c, value: [v ?? "", Array.isArray(c.value) ? c.value[1] : ""] })} label="From" />
          <Hint>and</Hint>
          <DateInput value={Array.isArray(c.value) ? c.value[1] : ""} onChange={(v) => onChange({ ...c, value: [Array.isArray(c.value) ? c.value[0] : "", v ?? ""] })} label="To" />
        </>
      ) : (
        <DateInput value={c.value} onChange={(v) => onChange({ ...c, value: v ?? "" })} label="Date" />
      )}
      <WhereFilters where={c.where ?? []} onChange={(where) => onChange({ ...c, where })} />
    </>
  );
}

const churnLevels = ["low", "medium", "high"] as const;

function PredictionFields({ condition: c, onChange }: { condition: PredictionCondition; onChange: (c: Condition) => void }) {
  const meta = predictions.find((p) => p.value === c.measure);
  const setMeasure = (measure: PredictionCondition["measure"]) =>
    onChange(measure === "churn_risk" ? { ...c, measure, op: "in", value: ["high"] } : { ...c, measure, op: "gte", value: c.measure === "churn_risk" ? 0 : c.value });
  return (
    <>
      <Select value={c.measure} onChange={(e) => setMeasure(e.target.value as PredictionCondition["measure"])} className={small} aria-label="Prediction">
        {predictions.map((p) => (
          <option key={p.value} value={p.value}>
            {p.label}
          </option>
        ))}
      </Select>
      {c.measure === "churn_risk" ? (
        <>
          <Hint>is</Hint>
          {churnLevels.map((level) => {
            const chosen = Array.isArray(c.value) ? (c.value as string[]) : typeof c.value === "string" ? [c.value] : [];
            return (
              <label key={level} className="flex items-center gap-1 text-xs text-zinc-700">
                <input
                  type="checkbox"
                  className="size-3.5 accent-accent-700"
                  checked={chosen.includes(level)}
                  onChange={(e) => onChange({ ...c, op: "in", value: e.target.checked ? [...chosen, level] : chosen.filter((x) => x !== level) })}
                />
                {level}
              </label>
            );
          })}
        </>
      ) : (
        <>
          <Hint>is</Hint>
          <CmpSelect value={c.op === "in" ? "gte" : c.op} onChange={(op) => onChange({ ...c, op, value: reshape(op, c.value) })} />
          <CmpValue op={c.op === "in" ? "gte" : c.op} value={c.value} onChange={(value) => onChange({ ...c, value })} prefix={meta?.money ? "$" : undefined} />
          {meta?.days ? <Hint>days</Hint> : null}
        </>
      )}
      <Hint>from</Hint>
      <Input value={c.metric ?? ""} onChange={(e) => onChange({ ...c, metric: e.target.value || undefined })} list="pl-metric-names" placeholder="Placed Order" className="h-8 w-32 text-xs" aria-label="Order metric" />
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
    else if (op === "in" || op === "not_in") value = Array.isArray(value) ? value : value === undefined || value === "" ? [] : [String(value)];
    else if (op === "between") value = Array.isArray(value) && value.length === 2 ? value : ["", ""];
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
      <Select value={filter.op} onChange={(e) => setOp(e.target.value as PropOp)} className={small} aria-label="Operator">
        {propOps.map((o) => (
          <option key={o.value} value={o.value}>
            {o.label}
          </option>
        ))}
      </Select>
      <ValueInput filter={filter} onChange={(value) => onChange({ ...filter, value })} />
      {filter.op === "within_last_days" || filter.op === "more_than_days_ago" || filter.op === "anniversary_within_days" ? <Hint>days</Hint> : null}
    </>
  );
}

/** A number when the text is one, otherwise the text (dates stay strings). */
const numberOrText = (s: string): string | number => (s.trim() !== "" && !Number.isNaN(Number(s)) ? Number(s) : s);

function ValueInput({ filter, onChange }: { filter: FieldFilter; onChange: (v: unknown) => void }) {
  if (isValuelessOp(filter.op)) return null;
  if (numericOps.has(filter.op)) {
    return <NumInput value={filter.value} onChange={onChange} label="Value" className="w-24" />;
  }
  if (filter.op === "between") {
    const [lo, hi] = Array.isArray(filter.value) ? filter.value : ["", ""];
    const text = (v: unknown) => (v === undefined || v === null ? "" : String(v));
    return (
      <>
        <Input value={text(lo)} onChange={(e) => onChange([numberOrText(e.target.value), hi])} placeholder="from (number or YYYY-MM-DD)" className="h-8 w-40 text-xs" aria-label="From" />
        <Hint>and</Hint>
        <Input value={text(hi)} onChange={(e) => onChange([lo, numberOrText(e.target.value)])} placeholder="to" className="h-8 w-40 text-xs" aria-label="To" />
      </>
    );
  }
  if (filter.op === "in" || filter.op === "not_in") {
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
