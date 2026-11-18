import { Field, Input, Select } from "../../components/ui";
import { useObjectTypes } from "./api";
import { ObjectFilters } from "./ObjectFilters";
import { fieldLabel } from "./parts";
import type { ObjectDateTrigger, ObjectTrigger } from "./types";

const hours = Array.from({ length: 24 }, (_, h) => h);
const hourLabel = (h: number) => new Date(2000, 0, 1, h).toLocaleTimeString([], { hour: "numeric" });

function TypePicker({ value, onChange }: { value: string; onChange: (key: string) => void }) {
  const types = useObjectTypes();
  const list = types.data?.data ?? [];
  return (
    <Field label="Object type" hint={list.length ? undefined : "Object types appear once objects arrive through the API, MCP or an integration."}>
      <Select value={value} onChange={(e) => onChange(e.target.value)}>
        {value === "" ? <option value="">Choose a type…</option> : null}
        {list.map((t) => (
          <option key={t.id} value={t.key}>
            {t.name}
          </option>
        ))}
      </Select>
    </Field>
  );
}

function useFields(key: string) {
  const types = useObjectTypes();
  return (types.data?.data ?? []).find((t) => t.key === key)?.fields.filter((f) => !f.sensitive) ?? [];
}

function Relationship({ value, onChange }: { value?: string; onChange: (v: string | undefined) => void }) {
  return (
    <Field label="Who gets it" hint="People linked to the object. Name a relationship, e.g. guest, to send only to them.">
      <Input value={value ?? ""} onChange={(e) => onChange(e.target.value.trim() ? e.target.value : undefined)} placeholder="Everyone linked" />
    </Field>
  );
}

/** "When an object is created or updated" trigger. */
export function ObjectTriggerFields({ trigger: t, onChange }: { trigger: ObjectTrigger; onChange: (t: ObjectTrigger) => void }) {
  const fields = useFields(t.object_type);
  return (
    <>
      <TypePicker value={t.object_type} onChange={(object_type) => onChange({ type: "object", object_type, on: t.on })} />
      <Field label="When">
        <Select value={t.on ?? "any"} onChange={(e) => onChange({ ...t, on: e.target.value as ObjectTrigger["on"] })}>
          <option value="any">It's created or updated</option>
          <option value="created">It's created</option>
          <option value="updated">It's updated</option>
        </Select>
      </Field>
      {t.on !== "created" ? (
        <div className="grid grid-cols-1 gap-3 sm:grid-cols-2">
          <Field label="Only when this field changes">
            <Select
              value={t.field ?? ""}
              onChange={(e) => {
                const next: ObjectTrigger = { ...t, field: e.target.value || undefined };
                if (!next.field) delete next.to;
                onChange(next);
              }}
            >
              <option value="">Any field</option>
              {fields.map((f) => (
                <option key={f.key} value={f.key}>
                  {fieldLabel(f)}
                </option>
              ))}
            </Select>
          </Field>
          {t.field ? (
            <Field label="…to this value" hint="Leave empty for any change">
              <Input value={t.to === undefined ? "" : String(t.to)} onChange={(e) => onChange({ ...t, to: e.target.value === "" ? undefined : e.target.value })} placeholder="cancelled" />
            </Field>
          ) : null}
        </div>
      ) : null}
      <Relationship value={t.relationship} onChange={(relationship) => onChange({ ...t, relationship })} />
      <div className="space-y-2">
        <div className="text-xs font-medium text-zinc-700">Only objects where</div>
        <ObjectFilters filters={t.filters ?? []} onChange={(filters) => onChange({ ...t, filters })} fields={fields} listId="pl-trigger-object-fields" />
        <p className="text-xs text-zinc-500">Checked again before each step: people leave if the object stops matching (say its status became cancelled), is deleted or they're unlinked.</p>
      </div>
    </>
  );
}

/** "N days or hours before or after an object's date" trigger. */
export function ObjectDateTriggerFields({ trigger: t, onChange }: { trigger: ObjectDateTrigger; onChange: (t: ObjectDateTrigger) => void }) {
  const fields = useFields(t.object_type);
  const dates = fields.filter((f) => f.type === "date" || f.type === "datetime");
  const byHours = t.offset_hours !== undefined;
  return (
    <>
      <TypePicker value={t.object_type} onChange={(object_type) => onChange({ type: "object_date", object_type, field: "", offset_days: t.offset_days ?? -2, hour: t.hour ?? 9 })} />
      <Field label="Date field" hint="Each object starts the flow once for its date, so a guest with two reservations gets two runs. If the date moves, so does the send.">
        <Select value={t.field} onChange={(e) => onChange({ ...t, field: e.target.value })}>
          {t.field === "" ? <option value="">{dates.length ? "Choose a date field…" : "This type has no date fields"}</option> : null}
          {dates.map((f) => (
            <option key={f.key} value={f.key}>
              {fieldLabel(f)}
            </option>
          ))}
        </Select>
      </Field>
      <Field label="Timing">
        <Select
          value={byHours ? "hours" : "days"}
          onChange={(e) => {
            const next: ObjectDateTrigger = { ...t };
            if (e.target.value === "hours") {
              next.offset_hours = -3;
              delete next.offset_days;
              delete next.hour;
            } else {
              delete next.offset_hours;
              next.offset_days = -2;
              next.hour = 9;
            }
            onChange(next);
          }}
        >
          <option value="days">Days before or after, at an hour</option>
          <option value="hours">Hours before or after the exact time</option>
        </Select>
      </Field>
      {byHours ? (
        <Field label="Hours from the date" hint="-3 = three hours before">
          <Input type="number" min={-8760} max={8760} value={t.offset_hours ?? 0} onChange={(e) => onChange({ ...t, offset_hours: Number(e.target.value) || 0 })} />
        </Field>
      ) : (
        <div className="grid grid-cols-2 gap-3">
          <Field label="Days from the date" hint="-2 = two days before">
            <Input type="number" min={-365} max={365} value={t.offset_days ?? 0} onChange={(e) => onChange({ ...t, offset_days: Number(e.target.value) || 0 })} />
          </Field>
          <Field label="Hour" hint="In your account's time zone">
            <Select value={t.hour ?? 9} onChange={(e) => onChange({ ...t, hour: Number(e.target.value) })}>
              {hours.map((h) => (
                <option key={h} value={h}>
                  {hourLabel(h)}
                </option>
              ))}
            </Select>
          </Field>
        </div>
      )}
      <Relationship value={t.relationship} onChange={(relationship) => onChange({ ...t, relationship })} />
      <div className="space-y-2">
        <div className="text-xs font-medium text-zinc-700">Only objects where</div>
        <ObjectFilters filters={t.filters ?? []} onChange={(filters) => onChange({ ...t, filters })} fields={fields} listId="pl-trigger-object-fields" />
        <p className="text-xs text-zinc-500">Checked when it's due and before each step, e.g. status equals confirmed, so cancelled ones are skipped.</p>
      </div>
    </>
  );
}
