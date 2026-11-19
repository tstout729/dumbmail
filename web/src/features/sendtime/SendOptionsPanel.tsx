import { Clock, Globe2, Hourglass, Send, Sparkles } from "lucide-react";
import type { ReactNode } from "react";
import type { Audience, Channel } from "../../api";
import { Field, Input, Select, Skeleton, Toggle, cx } from "../../components/ui";
import { formatNumber, pluralize } from "../../lib/format";
import { useSendPreview } from "./api";
import { clockLabel, forPreview, withSmartSending, withTiming } from "./options";
import type { SendOptions, SendPlanPreview, Timing } from "./types";

type Props = {
  campaignId: string;
  channel: Channel;
  audience: Audience;
  value: SendOptions | null;
  onChange: (value: SendOptions | null) => void;
  disabled: boolean;
  /** The account's time zone, for showing release times. */
  timezone: string;
};

const modes: { value: Timing; title: string; body: string; icon: ReactNode }[] = [
  { value: "all_at_once", title: "Everyone at once", body: "Goes out to everyone when you send or at the scheduled time.", icon: <Send className="size-4" /> },
  { value: "local_time", title: "Each person's local time", body: "Arrives at the same hour on each person's clock, wherever they are.", icon: <Globe2 className="size-4" /> },
  { value: "best_time", title: "Best time for each person", body: "Each person gets it at the hour they usually open or click.", icon: <Sparkles className="size-4" /> },
  { value: "gradual", title: "Spread out", body: "Sends in equal batches, one an hour, to ease a new domain or your support inbox.", icon: <Hourglass className="size-4" /> },
];

const spreadChoices = [2, 3, 4, 6, 8, 12, 24, 48, 72];
const withinChoices = [12, 24, 48, 72];
const smartChoices = [4, 8, 12, 16, 24, 48, 72];

/** How the campaign goes out: timing, smart sending, and an estimate of who gets it when. */
export function SendOptionsPanel({ campaignId, channel, audience, value, onChange, disabled, timezone }: Props) {
  const timing = value?.timing ?? "all_at_once";
  const set = (patch: Partial<SendOptions>) => value && onChange({ ...value, ...patch });
  const preview = useSendPreview({ campaign_id: campaignId, channel, audience, send_options: forPreview(value) });
  const what = channel === "sms" ? "text" : channel === "push" ? "notification" : channel === "whatsapp" ? "WhatsApp message" : "email";

  return (
    <div className="space-y-5">
      <div className="grid grid-cols-1 gap-2 sm:grid-cols-2" role="radiogroup" aria-label="When it goes out">
        {modes.map((m) => (
          <button
            key={m.value}
            type="button"
            role="radio"
            aria-checked={timing === m.value}
            disabled={disabled}
            onClick={() => onChange(withTiming(value, m.value))}
            className={cx(
              "flex items-start gap-3 rounded-lg border px-3 py-2.5 text-left transition-colors disabled:cursor-not-allowed disabled:opacity-60",
              timing === m.value ? "border-accent-600 bg-accent-50/60 ring-1 ring-accent-600" : "border-zinc-200 bg-white hover:border-zinc-300",
            )}
          >
            <span className={cx("mt-0.5 shrink-0", timing === m.value ? "text-accent-700" : "text-zinc-400")}>{m.icon}</span>
            <span className="min-w-0">
              <span className="block text-sm font-medium text-zinc-900">{m.title}</span>
              <span className="block text-xs text-zinc-500">{m.body}</span>
            </span>
          </button>
        ))}
      </div>

      {value?.timing === "local_time" ? (
        <div className="grid grid-cols-1 gap-4 sm:grid-cols-2">
          <Field label="Time on each person's clock" hint="The day is the one you schedule it for (or today when you send now).">
            <Input type="time" value={value.local_time ?? "10:00"} onChange={(e) => e.target.value && set({ local_time: e.target.value })} disabled={disabled} className="w-36" />
          </Field>
          <Field label="If that time already passed for someone" hint="Sending starts when the first time zone reaches it, so a full day of zones takes about 26 hours.">
            <Select value={value.late ?? "next_day"} onChange={(e) => set({ late: e.target.value === "immediately" ? "immediately" : "next_day" })} disabled={disabled}>
              <option value="next_day">Send at that time the next day</option>
              <option value="immediately">Send right away</option>
            </Select>
          </Field>
        </div>
      ) : null}

      {value?.timing === "best_time" ? (
        <div className="space-y-3">
          <div className="flex flex-wrap items-end gap-3">
            <Field label="Between">
              <Input type="time" value={value.window_start ?? "08:00"} onChange={(e) => e.target.value && set({ window_start: e.target.value })} disabled={disabled} className="w-32" />
            </Field>
            <Field label="And">
              <Input type="time" value={value.window_end ?? "20:00"} onChange={(e) => e.target.value && set({ window_end: e.target.value })} disabled={disabled} className="w-32" />
            </Field>
            <Field label="Within">
              <Select value={String(value.within_hours ?? 24)} onChange={(e) => set({ within_hours: Number(e.target.value) })} disabled={disabled} className="w-40">
                {withinChoices.map((h) => (
                  <option key={h} value={h}>
                    {h} hours
                  </option>
                ))}
              </Select>
            </Field>
          </div>
          <p className="text-xs text-zinc-500">
            Picked from each person's own opens and clicks over the last six months, in their time zone. People with little history get your account's best hour, then
            10:00. Apple Mail opens some emails on its own, so clicks count more than opens.
          </p>
        </div>
      ) : null}

      {value?.timing === "gradual" ? (
        <Field label="Spread over" hint="The audience is split into equal batches, one an hour.">
          <Select value={String(value.spread_hours ?? 4)} onChange={(e) => set({ spread_hours: Number(e.target.value) })} disabled={disabled} className="w-40">
            {spreadChoices.map((h) => (
              <option key={h} value={h}>
                {h} hours
              </option>
            ))}
          </Select>
        </Field>
      ) : null}

      <div className="flex items-start justify-between gap-4 border-t border-zinc-100 pt-4">
        <div className="min-w-0">
          <div className="text-sm font-medium text-zinc-900">Smart sending</div>
          <p className="text-xs text-zinc-500">
            Skip people who got {channel === "sms" ? "a text" : channel === "push" ? "a push notification" : channel === "whatsapp" ? "a WhatsApp message" : "an email"} from any campaign or flow recently. They show as skipped in the report.
          </p>
          {value?.smart_sending ? (
            <div className="mt-2 flex items-center gap-2 text-sm text-zinc-700">
              <span>In the last</span>
              <Select
                aria-label="Smart sending window"
                value={String(value.smart_sending_hours ?? 16)}
                onChange={(e) => onChange(withSmartSending(value, true, Number(e.target.value)))}
                disabled={disabled}
                className="w-28"
              >
                {smartChoices.map((h) => (
                  <option key={h} value={h}>
                    {h} hours
                  </option>
                ))}
              </Select>
            </div>
          ) : null}
        </div>
        <Toggle checked={value?.smart_sending ?? false} onChange={(on) => onChange(withSmartSending(value, on))} label="Smart sending" disabled={disabled} />
      </div>

      <PlanPreview preview={preview.data} loading={preview.isPending && audience.include.length > 0} error={preview.error?.message} timezone={timezone} what={what} hasAudience={audience.include.length > 0} />
    </div>
  );
}

const shownBuckets = 8;

function PlanPreview({ preview, loading, error, timezone, what, hasAudience }: { preview: SendPlanPreview | undefined; loading: boolean; error?: string; timezone: string; what: string; hasAudience: boolean }) {
  if (!hasAudience) return null;
  const fmt = (iso: string) => new Intl.DateTimeFormat("en-US", { timeZone: timezone, weekday: "short", hour: "numeric", minute: "2-digit" }).format(new Date(iso));
  const buckets = preview?.buckets ?? [];
  const max = Math.max(1, ...buckets.map((b) => b.count));
  return (
    <div className="rounded-lg bg-zinc-50 p-4">
      <div className="flex items-center gap-2 text-xs font-medium text-zinc-500">
        <Clock className="size-3.5" /> When it goes out
      </div>
      {error ? <p className="mt-2 text-xs text-red-600">{error}</p> : null}
      {loading ? (
        <div className="mt-3 space-y-2">
          {[0, 1, 2].map((i) => (
            <Skeleton key={i} className="h-5" />
          ))}
        </div>
      ) : preview ? (
        <>
          <p className="mt-1 text-sm text-zinc-700">{preview.description}</p>
          {buckets.length > 1 ? (
            <ul className="mt-3 space-y-1.5">
              {buckets.slice(0, shownBuckets).map((b) => (
                <li key={b.at} className="grid grid-cols-[6.5rem_minmax(0,1fr)_3.5rem] items-center gap-2 text-xs">
                  <span className="tabular-nums text-zinc-600">{fmt(b.at)}</span>
                  <span className="min-w-0">
                    <span className="block h-1.5 rounded-full bg-accent-600" style={{ width: `${Math.max(4, (b.count / max) * 100)}%` }} aria-hidden="true" />
                    {b.zones.length ? <span className="mt-0.5 block truncate text-[11px] text-zinc-400">{b.zones.map((z) => z.replace(/_/g, " ")).join(", ")}</span> : null}
                  </span>
                  <span className="text-right tabular-nums text-zinc-700">{formatNumber(b.count)}</span>
                </li>
              ))}
            </ul>
          ) : null}
          {buckets.length > shownBuckets ? <p className="mt-2 text-xs text-zinc-500">…and {pluralize(buckets.length - shownBuckets, "more release time")} after that.</p> : null}
          <p className="mt-3 text-xs text-zinc-500">
            {buckets.length > 1 && preview.starts_at && preview.ends_at
              ? `First ${what}s ${fmt(preview.starts_at)}, last ${fmt(preview.ends_at)} (your time). `
              : null}
            {preview.sampled && preview.sampled < preview.total ? `Estimated from ${formatNumber(preview.sampled)} of ${formatNumber(preview.total)} recipients. ` : null}
            {preview.smart_sending_skips !== null && preview.smart_sending_skips !== undefined
              ? `About ${pluralize(preview.smart_sending_skips, "person", "people")} would be skipped by smart sending right now.`
              : null}
          </p>
        </>
      ) : null}
    </div>
  );
}

/** One line for the step summary and the schedule panel. */
export function localTimeNote(o: SendOptions | null): string | null {
  return o?.timing === "local_time" ? `Goes out at ${clockLabel(o.local_time ?? "10:00")} in each person's time zone.` : null;
}
