import { ShieldAlert } from "lucide-react";
import type { DelayNode, DelayUnit, FlowTrigger } from "../../api/types";
import { Checkbox, Field, Input, Notice, Segmented, Select, cx } from "../../components/ui";
import { useMetricTrust } from "./api";
import type { DelayUntil, Weekday } from "./types";
import { weekdays } from "./summaries";

type Props = { node: DelayNode; onChange: (n: DelayNode) => void; trigger?: FlowTrigger; readOnly?: boolean };

const units = (
  <>
    <option value="minutes">minutes</option>
    <option value="hours">hours</option>
    <option value="days">days</option>
  </>
);

/** Drop options that are off, so a plain delay is saved exactly as before. */
function clean(n: DelayNode): DelayNode {
  const out: DelayNode = { ...n };
  if (!out.at) delete out.at;
  if (!out.weekdays?.length) delete out.weekdays;
  if (!out.until) delete out.until;
  return out;
}

function splitField(field: string): { source: "event" | "profile"; name: string } {
  if (field.startsWith("profile.")) return { source: "profile", name: field.slice("profile.".length) };
  return { source: "event", name: field.startsWith("event.") ? field.slice("event.".length) : field };
}

/** A delay: a set time, or until a date in the event or the profile, optionally at a time of day and on chosen weekdays. */
export function DelayEditor({ node, onChange, trigger, readOnly }: Props) {
  const set = (patch: Partial<DelayNode>) => onChange(clean({ ...node, ...patch }));
  const mode = node.until ? "until" : "time";
  const until = node.until;
  const setUntil = (patch: Partial<DelayUntil>) => until && set({ until: { ...until, ...patch } });
  const field = splitField(until?.field ?? "");
  const metric = trigger?.type === "metric" ? trigger.metric : null;
  const trust = useMetricTrust(until && field.source === "event" ? metric : null);
  const days = node.weekdays ?? [];
  const toggleDay = (d: Weekday) => set({ weekdays: days.includes(d) ? days.filter((x) => x !== d) : [...days, d] });

  return (
    <fieldset disabled={readOnly} className="min-w-0 space-y-4">
      <Segmented
        value={mode}
        onChange={(m) => (m === "until" ? set({ amount: 0, unit: "days", until: { field: "event.", offset_amount: 1, offset_unit: "days", offset_direction: "before" } }) : set({ amount: 1, unit: "days", until: null }))}
        options={[
          { value: "time", label: "For a set time" },
          { value: "until", label: "Until a date" },
        ]}
      />
      {mode === "time" ? (
        <div className="space-y-2">
          <p className="text-sm text-zinc-600">Wait before moving to the next step.</p>
          <div className="flex gap-2">
            <Input type="number" min={1} value={node.amount} onChange={(e) => set({ amount: Math.max(1, Number(e.target.value) || 1) })} className="w-24" aria-label="Amount" />
            <Select value={node.unit} onChange={(e) => set({ unit: e.target.value as DelayUnit })} className="w-32" aria-label="Unit">
              {units}
            </Select>
          </div>
        </div>
      ) : until ? (
        <div className="space-y-3">
          <p className="text-sm text-zinc-600">Wait until a date from the event that started the flow (like a check-in date) or from their profile.</p>
          <div className="flex flex-wrap gap-2">
            <Select value={field.source} onChange={(e) => setUntil({ field: `${e.target.value}.${field.name}` })} className="w-32" aria-label="Where the date is">
              <option value="event">Event</option>
              <option value="profile">Profile</option>
            </Select>
            <Input value={field.name} onChange={(e) => setUntil({ field: `${field.source}.${e.target.value.trim()}` })} placeholder="check_in" className="min-w-0 flex-1" aria-label="Property" />
          </div>
          <div className="flex flex-wrap items-center gap-2">
            <Input type="number" min={0} value={until.offset_amount ?? 0} onChange={(e) => setUntil({ offset_amount: Math.max(0, Number(e.target.value) || 0) })} className="w-20" aria-label="Offset" />
            <Select value={until.offset_unit ?? "days"} onChange={(e) => setUntil({ offset_unit: e.target.value as DelayUnit })} className="w-28" aria-label="Offset unit">
              {units}
            </Select>
            <Select value={until.offset_direction ?? "after"} onChange={(e) => setUntil({ offset_direction: e.target.value as "before" | "after", if_past: undefined })} className="w-28" aria-label="Before or after">
              <option value="before">before</option>
              <option value="after">after</option>
            </Select>
            <span className="text-sm text-zinc-600">the date</span>
          </div>
          <Field label="If that time has already passed">
            <Select
              value={until.if_past ?? ((until.offset_direction ?? "after") === "before" ? "exit" : "continue")}
              onChange={(e) => setUntil({ if_past: e.target.value as "continue" | "exit" })}
            >
              <option value="exit">Take them out of the flow</option>
              <option value="continue">Go on to the next step now</option>
            </Select>
          </Field>
          {trust.data?.browser_can_send ? (
            <Notice tone="amber" icon={<ShieldAlert />} title="Your website can send this event">
              Anyone can send “{metric}” from a browser, so its dates can't be trusted. Use it for timing only.
            </Notice>
          ) : null}
        </div>
      ) : null}

      <div className="space-y-3 border-t border-zinc-100 pt-3">
        <Checkbox label="At a time of day" checked={!!node.at} onChange={(e) => set({ at: e.target.checked ? "10:00" : null })} />
        {node.at ? (
          <Input type="time" value={node.at} onChange={(e) => set({ at: e.target.value || "10:00" })} className="w-32" aria-label="Time of day" />
        ) : null}
        <Checkbox label="Only on some days" checked={days.length > 0} onChange={(e) => set({ weekdays: e.target.checked ? ["tue", "wed", "thu"] : null })} />
        {days.length > 0 ? (
          <div className="flex flex-wrap gap-1" role="group" aria-label="Days">
            {weekdays.map((d) => (
              <button
                key={d.value}
                type="button"
                aria-pressed={days.includes(d.value)}
                onClick={() => toggleDay(d.value)}
                className={cx(
                  "rounded-md border px-2.5 py-1 text-xs font-medium",
                  days.includes(d.value) ? "border-accent-600 bg-accent-50 text-accent-800" : "border-zinc-200 text-zinc-600 hover:border-zinc-400",
                )}
              >
                {d.label}
              </button>
            ))}
          </div>
        ) : null}
        {node.at || days.length > 0 || node.until ? (
          <p className="text-xs text-zinc-500">
            Times use each person's time zone when we know it, otherwise your account's. If the time has passed that day, they wait for the next one. Once someone is waiting, their time
            stays put.
          </p>
        ) : null}
      </div>
    </fieldset>
  );
}
