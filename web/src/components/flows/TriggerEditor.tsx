import { Plus, X } from "lucide-react";
import type { FieldFilter, FlowTrigger, List, Segment } from "../../api";
import { useMetrics } from "../../api";
import { FilterFields } from "../segments/ConditionBuilder";
import { ObjectDateTriggerFields, ObjectTriggerFields } from "../../features/objects/ObjectTriggerFields";
import { Button, Field, IconButton, Input, Select, TagInput } from "../ui";

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
    case "event_date":
      return { type, metric: "", field: "", offset_days: -2, hour: 9 };
    case "object":
      return { type, object_type: "", on: "any" };
    case "object_date":
      return { type, object_type: "", field: "", offset_days: -2, hour: 9 };
  }
};

function TriggerFilters({ filters, onChange }: { filters: FieldFilter[]; onChange: (f: FieldFilter[]) => void }) {
  return (
    <div className="space-y-2">
      <div className="text-xs font-medium text-zinc-700">Trigger filters</div>
      {filters.map((f, i) => (
        <div key={i} className="flex flex-wrap items-center gap-1.5 rounded-md border border-zinc-200 p-2">
          <FilterFields filter={f} fieldList={undefined} fieldPlaceholder='"value" or event property' onChange={(nf) => onChange(filters.map((x, j) => (j === i ? nf : x)))} />
          <IconButton label="Remove filter" size="sm" onClick={() => onChange(filters.filter((_, j) => j !== i))}>
            <X className="size-3" />
          </IconButton>
        </div>
      ))}
      <Button size="sm" variant="ghost" icon={<Plus className="size-3" />} onClick={() => onChange([...filters, { field: "value", op: "gt", value: 0 }])}>
        Add filter
      </Button>
    </div>
  );
}

/** "Stop if cancelled": a later event of these metrics with the same key value takes people out. */
function CancelledByFields({ keyName, cancelledBy, onChange }: { keyName?: string; cancelledBy?: string[]; onChange: (key: string | undefined, cancelledBy: string[]) => void }) {
  return (
    <div className="space-y-3 rounded-md border border-zinc-200 p-3">
      <div>
        <div className="text-xs font-medium text-zinc-700">Stop if cancelled</div>
        <p className="mt-0.5 text-xs text-zinc-500">People leave the flow before the next step if one of these events happens later with the same value, or a newer trigger event replaces this one.</p>
      </div>
      <Field label="Cancelling events" hint="e.g. Reservation Cancelled">
        <TagInput value={cancelledBy ?? []} onChange={(v) => onChange(keyName, v)} placeholder="Add a metric" aria-label="Cancelling events" />
      </Field>
      <Field label="Matching property" hint="The event property both share, e.g. reservation_id">
        <Input value={keyName ?? ""} onChange={(e) => onChange(e.target.value.trim() || undefined, cancelledBy ?? [])} placeholder="reservation_id" />
      </Field>
    </div>
  );
}

function withCancel<T extends FlowTrigger & { key?: string; cancelled_by?: string[] }>(t: T, key: string | undefined, cancelledBy: string[]): T {
  const next: T = { ...t, key, cancelled_by: cancelledBy };
  if (!key) delete next.key;
  if (cancelledBy.length === 0) delete next.cancelled_by;
  return next;
}

export function TriggerEditor({ trigger, onChange, lists, segments, readOnly }: Props) {
  const metrics = useMetrics();
  const metricList = (
    <datalist id="pl-trigger-metrics">
      {(metrics.data ?? []).map((m) => (
        <option key={m.id} value={m.name} />
      ))}
    </datalist>
  );
  return (
    <fieldset disabled={readOnly} className="min-w-0 space-y-4">
      <Field label="Trigger type">
        <Select value={trigger.type} onChange={(e) => onChange(defaults(e.target.value as FlowTrigger["type"], lists, segments))}>
          <option value="metric">Metric: when someone does something</option>
          <option value="list">List: when someone is added to a list</option>
          <option value="segment">Segment: when someone joins a segment</option>
          <option value="date_property">Date: yearly on a profile date (e.g. birthday)</option>
          <option value="event_date">Event date: days before or after a date in an event (e.g. check-in)</option>
          <option value="object">Object: when an object (e.g. a reservation) is created or changes</option>
          <option value="object_date">Object date: before or after an object's date (e.g. check-in)</option>
        </Select>
      </Field>
      {trigger.type === "metric" ? (
        <>
          <Field label="Metric" hint="Pick a tracked metric or type a new one.">
            <Input value={trigger.metric} onChange={(e) => onChange({ ...trigger, metric: e.target.value })} list="pl-trigger-metrics" placeholder="Started Checkout" />
            {metricList}
          </Field>
          <TriggerFilters filters={trigger.filters ?? []} onChange={(filters) => onChange({ ...trigger, filters })} />
          <CancelledByFields keyName={trigger.key} cancelledBy={trigger.cancelled_by} onChange={(k, c) => onChange(withCancel(trigger, k, c))} />
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
      {trigger.type === "event_date" ? (
        <>
          <Field label="Event" hint="Each of these events starts the flow once, so two bookings mean two runs.">
            <Input value={trigger.metric} onChange={(e) => onChange({ ...trigger, metric: e.target.value })} list="pl-trigger-metrics" placeholder="Reservation Confirmed" />
            {metricList}
          </Field>
          <Field label="Date property in the event">
            <Input value={trigger.field} onChange={(e) => onChange({ ...trigger, field: e.target.value })} placeholder="check_in" />
          </Field>
          <div className="grid grid-cols-2 gap-3">
            <Field label="Days from the date" hint="-2 = two days before">
              <Input type="number" min={-365} max={365} value={trigger.offset_days ?? 0} onChange={(e) => onChange({ ...trigger, offset_days: Number(e.target.value) || 0 })} />
            </Field>
            <Field label="Hour" hint="In your account's time zone">
              <Select value={trigger.hour ?? 9} onChange={(e) => onChange({ ...trigger, hour: Number(e.target.value) })}>
                {Array.from({ length: 24 }, (_, h) => (
                  <option key={h} value={h}>
                    {new Date(2000, 0, 1, h).toLocaleTimeString([], { hour: "numeric" })}
                  </option>
                ))}
              </Select>
            </Field>
          </div>
          <TriggerFilters filters={trigger.filters ?? []} onChange={(filters) => onChange({ ...trigger, filters })} />
          <CancelledByFields keyName={trigger.key} cancelledBy={trigger.cancelled_by} onChange={(k, c) => onChange(withCancel(trigger, k, c))} />
        </>
      ) : null}
      {trigger.type === "object" ? <ObjectTriggerFields trigger={trigger} onChange={onChange} /> : null}
      {trigger.type === "object_date" ? <ObjectDateTriggerFields trigger={trigger} onChange={onChange} /> : null}
    </fieldset>
  );
}
