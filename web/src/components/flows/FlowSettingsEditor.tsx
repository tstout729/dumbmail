import type { FlowSettings, Reentry, SegmentDefinition } from "../../api";
import { ConditionBuilder } from "../segments/ConditionBuilder";
import { Field, Input, Select } from "../ui";

type Props = { settings: FlowSettings; onChange: (s: FlowSettings) => void; readOnly?: boolean };

const reentryMode = (r: Reentry | undefined) => (r === undefined || r === "never" ? "never" : r === "always" ? "always" : "after");

const emptyToUndefined = (d: SegmentDefinition): SegmentDefinition | undefined => (d.conditions.length ? d : undefined);

export function FlowSettingsEditor({ settings, onChange, readOnly }: Props) {
  const mode = reentryMode(settings.reentry);
  return (
    <fieldset disabled={readOnly} className="min-w-0 space-y-6">
      <div className="space-y-2">
        <Field label="Re-entry" hint="Can someone go through this flow more than once?">
          <Select
            value={mode}
            onChange={(e) => {
              const m = e.target.value;
              onChange({ ...settings, reentry: m === "after" ? { after_days: 30 } : (m as "never" | "always") });
            }}
          >
            <option value="never">Only once, ever</option>
            <option value="always">Every time they trigger it</option>
            <option value="after">Again after a number of days</option>
          </Select>
        </Field>
        {typeof settings.reentry === "object" ? (
          <div className="flex items-center gap-2 text-sm text-zinc-600">
            After
            <Input type="number" min={1} value={settings.reentry.after_days} onChange={(e) => onChange({ ...settings, reentry: { after_days: Math.max(1, Number(e.target.value) || 1) } })} className="w-20" />
            days
          </div>
        ) : null}
      </div>
      <Field label="Smart sending (hours)" hint="Skip a message if the person got any message in this many hours. Blank = off.">
        <Input
          type="number"
          min={0}
          value={settings.smart_sending_hours ?? ""}
          onChange={(e) => onChange({ ...settings, smart_sending_hours: e.target.value === "" ? undefined : Number(e.target.value) })}
          className="w-28"
        />
      </Field>
      <div>
        <div className="mb-1 text-xs font-medium text-zinc-700">Profile filter</div>
        <p className="mb-2 text-xs text-zinc-500">Only people who match can enter. Checked again before each step.</p>
        <ConditionBuilder
          value={settings.profile_filter ?? { match: "all", conditions: [] }}
          onChange={(d) => onChange({ ...settings, profile_filter: emptyToUndefined(d) })}
          emptyHint="Everyone who triggers the flow can enter."
        />
      </div>
      <div>
        <div className="mb-1 text-xs font-medium text-zinc-700">Exit condition</div>
        <p className="mb-2 text-xs text-zinc-500">People leave the flow as soon as they match (for example, they placed an order).</p>
        <ConditionBuilder
          value={settings.exit_condition ?? { match: "any", conditions: [] }}
          onChange={(d) => onChange({ ...settings, exit_condition: emptyToUndefined(d) })}
          emptyHint="No exit condition. People finish the whole flow."
        />
      </div>
    </fieldset>
  );
}
