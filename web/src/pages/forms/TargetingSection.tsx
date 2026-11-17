import { Crosshair, Monitor, PanelLeft, Smartphone, X } from "lucide-react";
import type { FormDevices, FormTrigger, FormVisitors, Targeting, Teaser, TeaserPosition, TeaserShow } from "../../api";
import { Checkbox, CodeBlock, CollapsibleCard, Field, IconButton, Input, Segmented, Select, SubHeading, TagInput, Toggle } from "../../components/ui";
import { tzAbbrev, utcToZoned, zonedToUtc } from "../../lib/tz";
import { contentDefaults, teaserPositions } from "./formConfig";
import { toInt, type Editor } from "./editorShared";

type SectionProps = { ed: Editor; open: boolean; onToggle: () => void };

const triggerLabels: Record<FormTrigger, string> = {
  delay: "After a delay",
  scroll: "After they scroll down the page",
  exit_intent: "When they're about to leave",
  manual: "Only when they click a button or the teaser",
};

export const openSnippet = (formId: string, text = "Get 10% off") => `<button type="button" data-dumbmail-open="${formId}">${text}</button>`;

function summary(ed: Editor, t: Targeting) {
  const trigger = t.trigger ?? "delay";
  const when =
    trigger === "delay"
      ? `After ${ed.config.delay_seconds ?? 5}s`
      : trigger === "scroll"
        ? `After ${t.scroll_percent ?? 40}% scroll`
        : trigger === "exit_intent"
          ? "On exit intent"
          : "On click only";
  const devices = t.devices === "desktop" ? "desktop" : t.devices === "mobile" ? "mobile" : "all devices";
  return `${when} · ${devices}${t.start_at || t.end_at ? " · scheduled" : ""}`;
}

export function TargetingSection({ ed, open, onToggle }: SectionProps) {
  const t: Targeting = ed.config.targeting ?? {};
  const set = (patch: Partial<Targeting>) => ed.set({ targeting: { ...t, ...patch } });
  const trigger = t.trigger ?? "delay";
  const tags = (v: string[]) => (v.length ? v : undefined);
  const tz = ed.timezone;
  const startMs = t.start_at ? Date.parse(t.start_at) : NaN;
  const endMs = t.end_at ? Date.parse(t.end_at) : NaN;
  const badRange = Number.isFinite(startMs) && Number.isFinite(endMs) && endMs <= startMs;
  return (
    <CollapsibleCard title="Targeting" icon={<Crosshair />} summary={summary(ed, t)} open={open} onToggle={onToggle}>
      <SubHeading>When it opens</SubHeading>
      <Field label="Show it">
        <Select value={trigger} onChange={(e) => set({ trigger: e.target.value as FormTrigger })}>
          {(Object.keys(triggerLabels) as FormTrigger[]).map((k) => (
            <option key={k} value={k}>
              {triggerLabels[k]}
            </option>
          ))}
        </Select>
      </Field>
      {trigger === "delay" ? (
        <Field label="Seconds after the page loads">
          <Input
            type="number"
            min={0}
            max={600}
            value={ed.config.delay_seconds ?? ""}
            placeholder="5"
            onChange={(e) => ed.set({ delay_seconds: e.target.value === "" ? undefined : toInt(e.target.value, 0, 600) })}
            className="w-28"
          />
        </Field>
      ) : trigger === "scroll" ? (
        <Field label="Percent of the page scrolled">
          <Input
            type="number"
            min={1}
            max={100}
            value={t.scroll_percent ?? ""}
            placeholder="40"
            onChange={(e) => set({ scroll_percent: e.target.value === "" ? undefined : toInt(e.target.value, 1, 100) })}
            className="w-28"
          />
        </Field>
      ) : trigger === "exit_intent" ? (
        <p className="text-xs text-zinc-500">Shows when the cursor heads for the tabs or back button. This needs a mouse, so it works on desktop.</p>
      ) : (
        <div className="space-y-2">
          <p className="text-xs text-zinc-500">
            It never opens on its own. Add this to any button or link on your site, or turn on the teaser below.
          </p>
          <CodeBlock code={openSnippet(ed.formId)} label="HTML" />
        </div>
      )}
      {trigger !== "manual" ? (
        <p className="text-xs text-zinc-500">
          Any element with <code className="rounded bg-zinc-100 px-1 font-mono">data-dumbmail-open=&quot;{ed.formId}&quot;</code> also opens it when clicked.
        </p>
      ) : null}

      <SubHeading>Who sees it</SubHeading>
      <div className="flex flex-wrap gap-x-6 gap-y-3">
        <div className="space-y-1">
          <div className="text-xs font-medium text-zinc-700">Devices</div>
          <Segmented
            value={t.devices ?? "all"}
            onChange={(devices: FormDevices) => set({ devices })}
            options={[
              { value: "all", label: "All" },
              { value: "desktop", label: <><Monitor className="size-3.5" /> Desktop</> },
              { value: "mobile", label: <><Smartphone className="size-3.5" /> Mobile</> },
            ]}
          />
        </div>
        <div className="space-y-1">
          <div className="text-xs font-medium text-zinc-700">Visitors</div>
          <Segmented
            value={t.visitors ?? "all"}
            onChange={(visitors: FormVisitors) => set({ visitors })}
            options={[
              { value: "all", label: "All" },
              { value: "new", label: "New" },
              { value: "returning", label: "Returning" },
            ]}
          />
        </div>
      </div>
      {t.visitors === "returning" ? <p className="-mt-2 text-xs text-zinc-500">Returning means this browser visited before today&rsquo;s visit.</p> : null}
      <Field label="After they've seen this many pages" hint="Counts pages in the current visit. 1 shows it on the first page.">
        <Input
          type="number"
          min={1}
          max={50}
          value={t.page_views ?? ""}
          placeholder="1"
          onChange={(e) => set({ page_views: e.target.value === "" ? undefined : toInt(e.target.value, 1, 50) })}
          className="w-28"
        />
      </Field>
      <Checkbox
        label="Hide from people who already subscribed"
        description="Skips visitors this browser already knows, e.g. from a past sign-up or an email click."
        checked={t.hide_for_subscribers ?? true}
        onChange={(e) => set({ hide_for_subscribers: e.target.checked })}
      />

      <SubHeading>Where</SubHeading>
      <Field label="Only on pages whose URL contains" hint="Press Enter after each. Leave empty to show on every page.">
        <TagInput value={t.url_contains ?? []} onChange={(v) => set({ url_contains: tags(v) })} placeholder="/products" />
      </Field>
      <Field label="Never on pages whose URL contains">
        <TagInput value={t.url_excludes ?? []} onChange={(v) => set({ url_excludes: tags(v) })} placeholder="/checkout" />
      </Field>

      <SubHeading>How often</SubHeading>
      <Field label="After someone closes it, wait" hint="Before it shows to them again. 0 shows it on every visit.">
        <div className="flex items-center gap-2">
          <Input
            type="number"
            min={0}
            max={365}
            value={t.frequency_days ?? ""}
            placeholder="7"
            onChange={(e) => set({ frequency_days: e.target.value === "" ? undefined : toInt(e.target.value, 0, 365) })}
            className="w-28"
          />
          <span className="text-sm text-zinc-600">days</span>
        </div>
      </Field>

      <SubHeading>Schedule</SubHeading>
      <p className="-mt-2 text-xs text-zinc-500">
        Optional. Outside these times the form doesn&rsquo;t show and its hosted page says it&rsquo;s closed. Times are in your account time zone (
        {tz.replace(/_/g, " ")}, {tzAbbrev(tz)}).
      </p>
      <div className="space-y-3">
        <DateTimeField label="Start" value={t.start_at} timezone={tz} onChange={(start_at) => set({ start_at })} />
        <DateTimeField label="End" value={t.end_at} timezone={tz} onChange={(end_at) => set({ end_at })} error={badRange ? "Pick a time after the start." : null} />
      </div>
    </CollapsibleCard>
  );
}

type DateTimeFieldProps = { label: string; value: string | undefined; timezone: string; onChange: (iso: string | undefined) => void; error?: string | null };

/** A datetime-local box read and written in the account time zone, stored as an ISO instant. */
function DateTimeField({ label, value, timezone, onChange, error }: DateTimeFieldProps) {
  const local = value && Number.isFinite(Date.parse(value)) ? (({ date, time }) => `${date}T${time}`)(utcToZoned(value, timezone)) : "";
  return (
    <Field label={label} error={error}>
      <div className="flex items-center gap-1">
        <Input
          type="datetime-local"
          value={local}
          onChange={(e) => {
            const [date, time] = e.target.value.split("T");
            onChange(date && time ? zonedToUtc(date, time, timezone) : undefined);
          }}
          className="w-56"
        />
        {value ? (
          <IconButton size="sm" label={`Clear ${label.toLowerCase()}`} onClick={() => onChange(undefined)}>
            <X className="size-3.5" />
          </IconButton>
        ) : null}
      </div>
    </Field>
  );
}

export function TeaserSection({ ed, open, onToggle }: SectionProps) {
  const t: Teaser = ed.config.teaser ?? {};
  const set = (patch: Partial<Teaser>) => ed.set({ teaser: { ...t, ...patch } });
  const enabled = t.enabled ?? false;
  return (
    <CollapsibleCard
      title="Teaser tab"
      icon={<PanelLeft />}
      summary={enabled ? `${teaserPositions.find((p) => p.value === (t.position ?? "bottom_left"))?.label} · ${t.show === "always" ? "always shown" : "after they close the form"}` : "Off"}
      open={open}
      onToggle={onToggle}
    >
      <div className="flex items-start justify-between gap-3">
        <p className="text-sm text-zinc-600">A small tab in a corner of the page that reopens the {ed.kind}. It disappears once they sign up.</p>
        <Toggle checked={enabled} onChange={(on) => set({ enabled: on })} label="Show a teaser tab" />
      </div>
      {enabled ? (
        <>
          <Field label="Tab text">
            <Input maxLength={40} value={t.text ?? ""} placeholder={(ed.config.headline || contentDefaults.headline).slice(0, 40)} onChange={(e) => set({ text: e.target.value || undefined })} />
          </Field>
          <Field label="Position">
            <Select value={t.position ?? "bottom_left"} onChange={(e) => set({ position: e.target.value as TeaserPosition })}>
              {teaserPositions.map((p) => (
                <option key={p.value} value={p.value}>
                  {p.label}
                </option>
              ))}
            </Select>
          </Field>
          <div className="space-y-1">
            <div className="text-xs font-medium text-zinc-700">Show it</div>
            <Segmented
              value={t.show ?? "after_close"}
              onChange={(show: TeaserShow) => set({ show })}
              options={[
                { value: "after_close", label: "After they close the form" },
                { value: "always", label: "Always" },
              ]}
            />
            <p className="text-xs text-zinc-500">
              {(t.show ?? "after_close") === "always"
                ? "Also shows before the form opens. Its own × hides it for the wait set under Targeting."
                : "Appears once they close the form without signing up."}
            </p>
          </div>
        </>
      ) : null}
    </CollapsibleCard>
  );
}
