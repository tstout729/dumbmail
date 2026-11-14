import { Plus, X } from "lucide-react";
import type { FlowTrigger, List, Segment } from "../../api";
import { useMetrics } from "../../api";
import { FilterFields } from "../segments/ConditionBuilder";
import { Button, Field, IconButton, Input, Select } from "../ui";

type Props = { trigger: FlowTrigger; onChange: (t: FlowTrigger) => void; lists: List[]; segments: Segment[]; readOnly?: boolean };

const defaults = (type: FlowTrigger["type"], lists: List[], segments: Segment[]): FlowTrigger => {
  switch (type) {
    case "metric":
      return { type, metric: "" };
    case "list":
      return { type, list_id: lists[0]?.id ?? "" };
    case "segment":
      return { type, segment_id: segments[0]?.id ?? "" };
    case "date_property":
      return { type, field: "birthday", offset_days: 0 };
  }
};

export function TriggerEditor({ trigger, onChange, lists, segments, readOnly }: Props) {
  const metrics = useMetrics();
  return (
    <fieldset disabled={readOnly} className="min-w-0 space-y-4">
      <Field label="Trigger type">
        <Select value={trigger.type} onChange={(e) => onChange(defaults(e.target.value as FlowTrigger["type"], lists, segments))}>
          <option value="metric">Metric: when someone does something</option>
          <option value="list">List: when someone is added to a list</option>
          <option value="segment">Segment: when someone joins a segment</option>
          <option value="date_property">Date: yearly on a profile date (e.g. birthday)</option>
        </Select>
      </Field>
      {trigger.type === "metric" ? (
        <>
          <Field label="Metric" hint="Pick a tracked metric or type a new one.">
            <Input value={trigger.metric} onChange={(e) => onChange({ ...trigger, metric: e.target.value })} list="pl-trigger-metrics" placeholder="Started Checkout" />
            <datalist id="pl-trigger-metrics">
              {(metrics.data ?? []).map((m) => (
                <option key={m.id} value={m.name} />
              ))}
            </datalist>
          </Field>
          <div className="space-y-2">
            <div className="text-xs font-medium text-zinc-700">Trigger filters</div>
            {(trigger.filters ?? []).map((f, i) => (
              <div key={i} className="flex flex-wrap items-center gap-1.5 rounded-md border border-zinc-200 p-2">
                <FilterFields
                  filter={f}
                  fieldList={undefined}
                  fieldPlaceholder='"value" or event property'
                  onChange={(nf) => onChange({ ...trigger, filters: (trigger.filters ?? []).map((x, j) => (j === i ? nf : x)) })}
                />
                <IconButton label="Remove filter" size="sm" onClick={() => onChange({ ...trigger, filters: (trigger.filters ?? []).filter((_, j) => j !== i) })}>
                  <X className="size-3" />
                </IconButton>
              </div>
            ))}
            <Button size="sm" variant="ghost" icon={<Plus className="size-3" />} onClick={() => onChange({ ...trigger, filters: [...(trigger.filters ?? []), { field: "value", op: "gt", value: 0 }] })}>
              Add filter
            </Button>
          </div>
        </>
      ) : null}
      {trigger.type === "list" ? (
        <Field label="List">
          <Select value={trigger.list_id} onChange={(e) => onChange({ ...trigger, list_id: e.target.value })}>
            {trigger.list_id === "" ? <option value="">Choose a list…</option> : null}
            {lists.map((l) => (
              <option key={l.id} value={l.id}>
                {l.name}
              </option>
            ))}
          </Select>
        </Field>
      ) : null}
      {trigger.type === "segment" ? (
        <Field label="Segment">
          <Select value={trigger.segment_id} onChange={(e) => onChange({ ...trigger, segment_id: e.target.value })}>
            {trigger.segment_id === "" ? <option value="">Choose a segment…</option> : null}
            {segments.map((s) => (
              <option key={s.id} value={s.id}>
                {s.name}
              </option>
            ))}
          </Select>
        </Field>
      ) : null}
      {trigger.type === "date_property" ? (
        <div className="grid grid-cols-2 gap-3">
          <Field label="Profile property">
            <Input value={trigger.field} onChange={(e) => onChange({ ...trigger, field: e.target.value })} placeholder="birthday" />
          </Field>
          <Field label="Offset (days)" hint="-7 = a week before">
            <Input type="number" value={trigger.offset_days ?? 0} onChange={(e) => onChange({ ...trigger, offset_days: Number(e.target.value) || 0 })} />
          </Field>
        </div>
      ) : null}
    </fieldset>
  );
}
