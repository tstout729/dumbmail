import { useId } from "react";
import { FlaskConical, Plus, Trash2 } from "lucide-react";
import type { AbMetric, AbTest, Channel, VersionId } from "../../api";
import { Badge, Button, Field, Select, Tabs } from "../../components/ui";
import { MAX_VERSIONS, afterTestSentence, metricLabels, metricsFor, splitSentence, testSettings, versionIds, versionLabel, waitLabel } from "./abTest";

/** Shown above the editor when the campaign has no test yet. */
export function AbTestStart({ onStart }: { onStart: () => void }) {
  return (
    <div className="mb-4 flex flex-wrap items-center justify-between gap-3 rounded-lg border border-dashed border-zinc-300 px-3.5 py-2.5">
      <p className="text-sm text-zinc-600">
        <span className="font-medium text-zinc-900">Not sure which version works best?</span> Send up to 4 versions to a slice of your audience, then send the
        winner to everyone else.
      </p>
      <Button size="sm" icon={<FlaskConical className="size-3.5" />} onClick={onStart}>
        A/B test
      </Button>
    </div>
  );
}

const waitPresets = [1, 2, 4, 8, 12, 24, 48, 72, 168];

const metricHints: Record<AbMetric, string> = {
  open_rate: "Best for testing subject lines.",
  click_rate: "Best for testing the message itself.",
  revenue: "Orders per person sent; needs enough sales to tell.",
};

type SettingsProps = {
  test: AbTest;
  channel: Channel;
  /** Reachable recipients, when the estimate is in. */
  total: number | undefined;
  disabled: boolean;
  onChange: (test: AbTest) => void;
  onRemove: () => void;
};

/** Test size, wait, winning metric and who picks, with a plain sentence on who gets what. */
export function AbTestSettings({ test, channel, total, disabled, onChange, onRemove }: SettingsProps) {
  const s = testSettings(test);
  const sizeId = useId();
  const everyone = s.test_percent >= 100;
  const set = (patch: Partial<AbTest>) => onChange({ ...test, ...s, ...patch });
  const waits = waitPresets.includes(s.wait_hours) ? waitPresets : [...waitPresets, s.wait_hours].sort((a, b) => a - b);
  const after = afterTestSentence(test);
  return (
    <div className="mb-4 rounded-lg border border-violet-200 bg-violet-50/40 p-3.5">
      <div className="mb-3 flex flex-wrap items-center justify-between gap-2">
        <div className="flex items-center gap-2 text-sm font-semibold text-zinc-900">
          <FlaskConical className="size-4 text-violet-700" /> A/B test <Badge tone="violet">{versionIds(test).length} versions</Badge>
        </div>
        {disabled ? null : (
          <Button size="sm" variant="ghost" onClick={onRemove}>
            Remove A/B test
          </Button>
        )}
      </div>
      <fieldset disabled={disabled} className="grid min-w-0 grid-cols-1 gap-3 sm:grid-cols-2 lg:grid-cols-4">
        <Field label={`Test size: ${s.test_percent}% of the audience`} htmlFor={sizeId} hint={everyone ? "Everyone is in the test." : "Split evenly across the versions."}>
          <input
            id={sizeId}
            type="range"
            min={5}
            max={100}
            step={5}
            value={s.test_percent}
            onChange={(e) => set({ test_percent: Number(e.target.value) })}
            className="h-8.5 w-full accent-accent-700"
          />
        </Field>
        {everyone ? null : (
          <Field label="Wait before picking" hint="Give people time to open and click.">
            <Select value={s.wait_hours} onChange={(e) => set({ wait_hours: Number(e.target.value) })}>
              {waits.map((h) => (
                <option key={h} value={h}>
                  {waitLabel(h)}
                </option>
              ))}
            </Select>
          </Field>
        )}
        <Field label="Winner is the best" hint={metricHints[s.metric]}>
          <Select value={s.metric} onChange={(e) => set({ metric: e.target.value as AbMetric })}>
            {metricsFor(channel).map((m) => (
              <option key={m} value={m}>
                {m === "revenue" ? "Revenue per person" : metricLabels[m]}
              </option>
            ))}
          </Select>
        </Field>
        {everyone ? null : (
          <Field label="Who picks the winner" hint={s.pick === "auto" ? "Sent as soon as the wait ends." : "You choose from the report."}>
            <Select value={s.pick} onChange={(e) => set({ pick: e.target.value as "auto" | "manual" })}>
              <option value="auto">Send the leader automatically</option>
              <option value="manual">I'll pick</option>
            </Select>
          </Field>
        )}
      </fieldset>
      <p className="mt-3 text-sm text-zinc-700">
        {splitSentence(test, total)} {after}
        {everyone ? " The report still shows which version did best." : null}
      </p>
    </div>
  );
}

type VersionBarProps = {
  versions: VersionId[];
  value: VersionId;
  onChange: (v: VersionId) => void;
  disabled: boolean;
  onAdd: () => void;
  onRemove: (v: VersionId) => void;
};

/** Version tabs over the editor, with add and remove. */
export function VersionBar({ versions, value, onChange, disabled, onAdd, onRemove }: VersionBarProps) {
  return (
    <div className="flex flex-wrap items-end justify-between gap-x-3">
      <Tabs value={value} onChange={onChange} tabs={versions.map((v) => ({ value: v, label: versionLabel(v) }))} />
      {disabled ? null : (
        <div className="mb-4 flex items-center gap-1">
          {versions.length < MAX_VERSIONS ? (
            <Button size="sm" variant="ghost" icon={<Plus className="size-3.5" />} onClick={onAdd} title={`Adds a copy of version ${value.toUpperCase()}`}>
              Add version
            </Button>
          ) : null}
          <Button size="sm" variant="ghost" icon={<Trash2 className="size-3.5" />} onClick={() => onRemove(value)}>
            Remove version {value.toUpperCase()}
          </Button>
        </div>
      )}
    </div>
  );
}
