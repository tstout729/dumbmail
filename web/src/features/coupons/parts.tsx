import { Badge, Field, Input, Segmented, Select, Textarea, Toggle } from "../../components/ui";
import { formatDate, formatNumber } from "../../lib/format";
import { utcToZoned, zonedToUtc } from "../../lib/tz";
import type { Coupon, CouponAlphabet, CouponInput } from "./types";

export type ExpiryKind = "none" | "date" | "days";

/** The editable settings of a coupon, as form state. */
export type CouponDraft = {
  name: string;
  description: string;
  source: "uploaded" | "generated";
  prefix: string;
  length: string;
  alphabet: CouponAlphabet;
  expiry: ExpiryKind;
  expiresOn: string;
  expiresAfterDays: string;
  newEachTime: boolean;
  lowStock: string;
  formPerHour: string;
};

export function draftFrom(c: Coupon | null, timeZone: string): CouponDraft {
  return {
    name: c?.name ?? "",
    description: c?.description ?? "",
    source: c?.source ?? "uploaded",
    prefix: c?.pattern?.prefix ?? "",
    length: String(c?.pattern?.length ?? 8),
    alphabet: c?.pattern?.alphabet ?? "letters_digits",
    expiry: c?.expires_at ? "date" : c?.expires_after_days ? "days" : "none",
    expiresOn: c?.expires_at ? utcToZoned(c.expires_at, timeZone).date : "",
    expiresAfterDays: String(c?.expires_after_days ?? 30),
    newEachTime: c?.new_code_each_time ?? false,
    lowStock: c?.low_stock_threshold ? String(c.low_stock_threshold) : "",
    formPerHour: String(c?.form_codes_per_hour ?? 200),
  };
}

const int = (s: string) => (/^\d+$/.test(s.trim()) ? Number(s.trim()) : NaN);

/** What's wrong with the draft, or null. */
export function draftProblem(d: CouponDraft, creating: boolean): string | null {
  if (creating && !/^[A-Za-z0-9][A-Za-z0-9 _.-]{0,63}$/.test(d.name.trim())) return "Name it with letters, digits, spaces, . _ or - (up to 64).";
  if (d.source === "generated") {
    const n = int(d.length);
    if (!(n >= 4 && n <= 24)) return "The random part must be 4 to 24 characters.";
    if (!/^[A-Za-z0-9_-]{0,20}$/.test(d.prefix)) return "The prefix can have up to 20 letters, digits, - and _.";
  }
  if (d.expiry === "date" && !/^\d{4}-\d{2}-\d{2}$/.test(d.expiresOn)) return "Pick the date codes stop working.";
  if (d.expiry === "days" && !(int(d.expiresAfterDays) >= 1 && int(d.expiresAfterDays) <= 3650)) return "Codes can last 1 to 3,650 days.";
  if (d.lowStock.trim() && !(int(d.lowStock) >= 1)) return "The low-stock alert needs a whole number.";
  if (!(int(d.formPerHour) >= 1 && int(d.formPerHour) <= 10000)) return "Sign-up forms can hand out 1 to 10,000 codes an hour.";
  return null;
}

export function inputFrom(d: CouponDraft, timeZone: string, creating: boolean): CouponInput {
  return {
    ...(creating ? { name: d.name.trim() } : {}),
    description: d.description.trim(),
    source: d.source,
    pattern: d.source === "generated" ? { prefix: d.prefix, length: int(d.length), alphabet: d.alphabet } : null,
    // A date means "works through that day" in the account's time zone.
    expires_at: d.expiry === "date" ? zonedToUtc(d.expiresOn, "23:59", timeZone) : null,
    expires_after_days: d.expiry === "days" ? int(d.expiresAfterDays) : null,
    new_code_each_time: d.newEachTime,
    low_stock_threshold: d.lowStock.trim() ? int(d.lowStock) : null,
    form_codes_per_hour: int(d.formPerHour),
  };
}

export function CouponSettings({ draft, onChange, creating }: { draft: CouponDraft; onChange: (d: CouponDraft) => void; creating: boolean }) {
  const set = (patch: Partial<CouponDraft>) => onChange({ ...draft, ...patch });
  return (
    <div className="space-y-4">
      {creating ? (
        <Field label="Name" hint="Your emails use it: {% coupon_code 'NAME' %}. It can't change later.">
          <Input value={draft.name} maxLength={64} onChange={(e) => set({ name: e.target.value })} placeholder="e.g. WELCOME10" className="font-mono" />
        </Field>
      ) : null}
      <Field label="Description" hint="For you; people don't see it.">
        <Textarea rows={2} maxLength={500} value={draft.description} onChange={(e) => set({ description: e.target.value })} placeholder="e.g. 10% off a first order" />
      </Field>
      <Field label="Where codes come from">
        <Segmented
          value={draft.source}
          onChange={(source) => set({ source })}
          options={[
            { value: "uploaded", label: "I upload them" },
            { value: "generated", label: "DumbMail makes them" },
          ]}
        />
      </Field>
      {draft.source === "uploaded" ? (
        <p className="-mt-2 text-xs text-zinc-500">Make single-use codes in your store, then paste or upload them here.</p>
      ) : (
        <div className="-mt-1 space-y-2">
          <p className="text-xs text-zinc-500">DumbMail makes random codes. Export them and import them into your store before you send, so they work at checkout.</p>
          <div className="grid grid-cols-1 gap-3 sm:grid-cols-3">
            <Field label="Prefix">
              <Input value={draft.prefix} maxLength={20} onChange={(e) => set({ prefix: e.target.value })} placeholder="WELCOME-" className="font-mono" />
            </Field>
            <Field label="Random characters">
              <Input value={draft.length} inputMode="numeric" onChange={(e) => set({ length: e.target.value })} />
            </Field>
            <Field label="Made of">
              <Select value={draft.alphabet} onChange={(e) => set({ alphabet: e.target.value as CouponAlphabet })}>
                <option value="letters_digits">Letters and digits</option>
                <option value="letters">Letters</option>
                <option value="digits">Digits</option>
              </Select>
            </Field>
          </div>
          <p className="text-xs text-zinc-500">Look-alike characters (0 and O, 1, I and L) are left out.</p>
        </div>
      )}
      <Field label="Expiry">
        <div className="flex flex-wrap items-center gap-2">
          <Select value={draft.expiry} onChange={(e) => set({ expiry: e.target.value as ExpiryKind })} className="w-auto">
            <option value="none">Codes don't expire</option>
            <option value="date">On a date</option>
            <option value="days">Days after someone gets one</option>
          </Select>
          {draft.expiry === "date" ? <Input type="date" value={draft.expiresOn} onChange={(e) => set({ expiresOn: e.target.value })} className="w-auto" aria-label="Expiry date" /> : null}
          {draft.expiry === "days" ? (
            <Input value={draft.expiresAfterDays} inputMode="numeric" onChange={(e) => set({ expiresAfterDays: e.target.value })} className="w-24" aria-label="Days" />
          ) : null}
        </div>
      </Field>
      <div className="flex items-start justify-between gap-4">
        <div>
          <div className="text-sm font-medium text-zinc-800">A new code in every message</div>
          <p className="text-xs text-zinc-500">Off: a person sees the same code in every email and text, including reminders.</p>
        </div>
        <Toggle checked={draft.newEachTime} onChange={(newEachTime) => set({ newEachTime })} label="A new code in every message" />
      </div>
      <div className="grid grid-cols-1 gap-3 sm:grid-cols-2">
        <Field label="Alert me when fewer than" hint="codes are left. Leave empty for no alert.">
          <Input value={draft.lowStock} inputMode="numeric" onChange={(e) => set({ lowStock: e.target.value })} placeholder="e.g. 500" />
        </Field>
        <Field label="Sign-up forms hand out at most" hint="codes an hour, so bots can't use them all up.">
          <Input value={draft.formPerHour} inputMode="numeric" onChange={(e) => set({ formPerHour: e.target.value })} />
        </Field>
      </div>
    </div>
  );
}

export function StockBadge({ c }: { c: Coupon }) {
  if (c.alerts.out_of_codes || (c.codes_total > 0 && c.stock.available === 0)) return <Badge tone="red">Out of codes</Badge>;
  if (c.codes_total === 0) return <Badge tone="gray">No codes yet</Badge>;
  if (c.alerts.low_stock) return <Badge tone="amber">Running low</Badge>;
  return <Badge tone="green">{formatNumber(c.stock.available)} left</Badge>;
}

export function expiryText(c: Coupon, timeZone: string): string {
  if (c.expires_at) return `Expires ${formatDate(c.expires_at, timeZone)}`;
  if (c.expires_after_days) return `Each code lasts ${formatNumber(c.expires_after_days)} days`;
  return "No expiry";
}
