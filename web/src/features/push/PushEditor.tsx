import { useRef, useState } from "react";
import { Loader2, Plus, Trash2 } from "lucide-react";
import { useSession } from "../../api";
import { VariableMenu, insertAtCursor } from "../../components/email/VariableMenu";
import { Button, Checkbox, CollapsibleCard, Field, IconButton, Input, Segmented, Select, Textarea, cx } from "../../components/ui";
import { useDebouncedValue } from "../../lib/hooks";
import { usePushRender } from "./api";
import type { PushContent, PushPlatform, PushRender, PushUrgency } from "./types";

const PLATFORMS: { id: PushPlatform; label: string }[] = [
  { id: "web", label: "Web browsers" },
  { id: "ios", label: "iOS app" },
  { id: "android", label: "Android app" },
];

type Props = { value: PushContent; onChange: (v: PushContent) => void; readOnly?: boolean };

/** Push content for a campaign or flow step, with a preview on each kind of device. */
export function PushEditor({ value, onChange, readOnly }: Props) {
  const titleRef = useRef<HTMLInputElement>(null);
  const bodyRef = useRef<HTMLTextAreaElement>(null);
  const [advanced, setAdvanced] = useState(false);
  const debounced = useDebouncedValue(value, 400);
  const render = usePushRender(debounced);
  const set = (patch: Partial<PushContent>) => onChange({ ...value, ...patch });
  const opt = (s: string) => (s.trim() === "" ? undefined : s);
  const platforms = value.platforms?.length ? value.platforms : PLATFORMS.map((p) => p.id);
  const togglePlatform = (id: PushPlatform, on: boolean) => {
    const next = on ? [...platforms, id] : platforms.filter((p) => p !== id);
    if (next.length === 0) return;
    set({ platforms: next.length === PLATFORMS.length ? undefined : next });
  };
  const actions = value.actions ?? [];

  return (
    <div className="grid grid-cols-1 gap-6 lg:grid-cols-[minmax(0,1fr)_340px]">
      <fieldset disabled={readOnly} className="min-w-0 space-y-4">
        <Field label="Title" hint={`${[...value.title].length}/100`}>
          <div className="flex gap-2">
            <Input ref={titleRef} value={value.title} maxLength={100} onChange={(e) => set({ title: e.target.value })} placeholder="New arrivals are in" aria-label="Title" />
            <VariableMenu compact onInsert={(v) => set({ title: insertAtCursor(titleRef.current, value.title, v) })} />
          </div>
        </Field>
        <Field label="Message" hint={`${[...value.body].length}/500`}>
          <div className="space-y-1">
            <Textarea ref={bodyRef} rows={3} value={value.body} maxLength={500} onChange={(e) => set({ body: e.target.value })} placeholder={'Hi {{ first_name|default("there") }}, take a look before they sell out.'} aria-label="Message" />
            <div className="flex justify-end">
              <VariableMenu onInsert={(v) => set({ body: insertAtCursor(bodyRef.current, value.body, v) })} />
            </div>
          </div>
        </Field>
        <Field label="Link" hint="Where a tap goes. Leave empty to open your website.">
          <Input value={value.url ?? ""} onChange={(e) => set({ url: opt(e.target.value) })} placeholder="https://yourbrand.com/new" />
        </Field>
        <Field label="Send to">
          <div className="flex flex-wrap gap-x-5 gap-y-2">
            {PLATFORMS.map((p) => (
              <Checkbox key={p.id} label={p.label} checked={platforms.includes(p.id)} onChange={(e) => togglePlatform(p.id, e.target.checked)} />
            ))}
          </div>
        </Field>
        <Field label="Image (optional)" hint="A large picture. Shows in Chrome, Edge and Android, and in iOS apps that support it.">
          <Input value={value.image ?? ""} onChange={(e) => set({ image: opt(e.target.value) })} placeholder="https://…/banner.jpg" />
        </Field>
        <div className="space-y-2">
          <div className="text-xs font-medium text-zinc-700">Buttons (optional)</div>
          <p className="text-xs text-zinc-500">Up to two. They show in Chrome and Edge, and in apps that add them.</p>
          {actions.map((a, i) => (
            <div key={i} className="flex flex-col gap-2 sm:flex-row">
              <Input
                value={a.title}
                maxLength={40}
                onChange={(e) => set({ actions: actions.map((x, j) => (j === i ? { ...x, title: e.target.value } : x)) })}
                placeholder="Shop now"
                aria-label={`Button ${i + 1} label`}
                className="sm:w-40"
              />
              <Input value={a.url} onChange={(e) => set({ actions: actions.map((x, j) => (j === i ? { ...x, url: e.target.value } : x)) })} placeholder="https://…" aria-label={`Button ${i + 1} link`} />
              <IconButton label="Remove button" onClick={() => set({ actions: actions.filter((_, j) => j !== i) })}>
                <Trash2 className="size-4" />
              </IconButton>
            </div>
          ))}
          {actions.length < 2 && !readOnly ? (
            <Button size="sm" variant="ghost" icon={<Plus className="size-3.5" />} onClick={() => set({ actions: [...actions, { title: "", url: "" }] })}>
              Add a button
            </Button>
          ) : null}
        </div>
        <CollapsibleCard title="More options" summary={advancedSummary(value)} open={advanced} onToggle={() => setAdvanced(!advanced)}>
          <div className="grid grid-cols-1 gap-3 sm:grid-cols-2">
            <Field label="Icon (web)" hint="Defaults to your logo.">
              <Input value={value.icon ?? ""} onChange={(e) => set({ icon: opt(e.target.value) })} placeholder="https://…/icon.png" />
            </Field>
            <Field label="Urgency">
              <Select value={value.urgency ?? "normal"} onChange={(e) => set({ urgency: e.target.value === "normal" ? undefined : (e.target.value as PushUrgency) })}>
                <option value="very_low">Very low</option>
                <option value="low">Low</option>
                <option value="normal">Normal</option>
                <option value="high">High</option>
              </Select>
            </Field>
            <Field label="Keep for offline devices" hint="Hours the push service holds it for a device that's off (up to 672).">
              <Input
                type="number"
                min={0}
                max={672}
                value={value.ttl_seconds === undefined ? 24 : Math.round(value.ttl_seconds / 3600)}
                onChange={(e) => set({ ttl_seconds: Math.min(672, Math.max(0, Number(e.target.value) || 0)) * 3600 })}
              />
            </Field>
            <Field label="Topic" hint="A newer notification with the same topic replaces one not yet delivered.">
              <Input value={value.topic ?? ""} maxLength={32} onChange={(e) => set({ topic: opt(e.target.value.replace(/[^A-Za-z0-9_-]/g, "")) })} placeholder="spring-sale" />
            </Field>
            <Field label="iOS sound" hint="Leave as default, or the name of a sound in your app.">
              <Input value={value.ios_sound ?? "default"} onChange={(e) => set({ ios_sound: e.target.value === "default" ? undefined : e.target.value })} />
            </Field>
            <Field label="iOS badge number">
              <Input type="number" min={0} max={9999} value={value.ios_badge ?? ""} onChange={(e) => set({ ios_badge: e.target.value === "" ? undefined : Math.max(0, Number(e.target.value) || 0) })} />
            </Field>
            <Field label="Android channel" hint="The notification channel id your app created.">
              <Input value={value.android_channel_id ?? ""} onChange={(e) => set({ android_channel_id: opt(e.target.value) })} placeholder="offers" />
            </Field>
          </div>
        </CollapsibleCard>
        <p className="text-xs text-zinc-500">Coupon codes can't go in push notifications. Send them by email or text.</p>
        {render.error ? <p className="text-xs text-red-600">{render.error.message}</p> : null}
      </fieldset>
      <PushPreview content={value} rendered={render.data ?? null} loading={render.isFetching} platforms={platforms} />
    </div>
  );
}

function advancedSummary(v: PushContent): string {
  const parts = [v.urgency && v.urgency !== "normal" ? `${v.urgency.replace("_", " ")} urgency` : null, v.topic ? `topic ${v.topic}` : null, v.ios_badge !== undefined ? `badge ${v.ios_badge}` : null].filter(Boolean);
  return parts.length ? parts.join(" · ") : "Icon, urgency, time to keep, topic, iOS and Android";
}

type Device = "web" | "ios" | "android";

/** How the notification looks on a desktop browser, an iPhone lock screen and an Android phone. */
export function PushPreview({ content, rendered, loading, platforms }: { content: PushContent; rendered: PushRender | null; loading?: boolean; platforms: PushPlatform[] }) {
  const { account } = useSession();
  const [device, setDevice] = useState<Device>(platforms[0] ?? "web");
  const shown: Device = platforms.includes(device) ? device : (platforms[0] ?? "web");
  const title = rendered?.title ?? content.title;
  const body = rendered?.body ?? content.body;
  const image = rendered?.image ?? content.image ?? null;
  const icon = rendered?.icon ?? account.logo_url ?? null;
  const site = (account.website ?? "yourbrand.com").replace(/^https?:\/\//, "").replace(/\/$/, "");
  const actions = (rendered?.actions ?? content.actions ?? []).filter((a) => a.title.trim() !== "");
  const empty = !title && !body;
  return (
    <div className="mx-auto w-full max-w-[340px] space-y-3">
      <div className="flex items-center justify-between gap-2">
        <Segmented
          size="sm"
          value={shown}
          onChange={setDevice}
          options={PLATFORMS.filter((p) => platforms.includes(p.id)).map((p) => ({ value: p.id, label: p.id === "web" ? "Browser" : p.id === "ios" ? "iPhone" : "Android" }))}
        />
        {loading ? <Loader2 className="size-3.5 animate-spin text-zinc-400" /> : null}
      </div>
      {shown === "web" ? (
        <div className="rounded-xl bg-zinc-100 p-4">
          <div className="rounded-lg bg-white p-3 shadow-md ring-1 ring-black/5">
            <div className="flex gap-3">
              {icon ? <img src={icon} alt="" className="size-10 shrink-0 rounded object-cover" /> : <div className="size-10 shrink-0 rounded bg-zinc-200" />}
              <div className="min-w-0 flex-1">
                <div className="truncate text-[13px] font-semibold text-zinc-900">{title || (empty ? "Your title" : "")}</div>
                <div className="line-clamp-3 break-words text-[12px] leading-snug text-zinc-600">{body || (empty ? "Your message" : "")}</div>
                <div className="mt-1 truncate text-[11px] text-zinc-400">{site}</div>
              </div>
            </div>
            {image ? <img src={image} alt="" className="mt-2 max-h-36 w-full rounded object-cover" /> : null}
            {actions.length ? (
              <div className="mt-2 flex gap-2 border-t border-zinc-100 pt-2">
                {actions.map((a, i) => (
                  <span key={i} className="flex-1 truncate rounded bg-zinc-100 px-2 py-1 text-center text-[12px] font-medium text-zinc-700">
                    {a.title}
                  </span>
                ))}
              </div>
            ) : null}
          </div>
        </div>
      ) : (
        <div className={cx("rounded-[2.2rem] border-[6px] border-zinc-900 px-3 pb-10 pt-8", shown === "ios" ? "bg-gradient-to-b from-indigo-400 to-sky-700" : "bg-zinc-800")}>
          <div className={cx("mb-6 text-center font-light text-white", shown === "ios" ? "text-5xl" : "text-left text-3xl")}>9:41</div>
          <div className={cx("p-3 shadow-lg", shown === "ios" ? "rounded-2xl bg-white/85 backdrop-blur" : "rounded-xl bg-zinc-100")}>
            <div className="flex items-center gap-1.5 text-[11px] text-zinc-500">
              <span className="flex size-4 items-center justify-center rounded bg-zinc-900 text-[8px] font-bold text-white">{account.name.slice(0, 1).toUpperCase()}</span>
              <span className="truncate font-medium uppercase tracking-wide">{account.name}</span>
              <span className="ml-auto">now</span>
            </div>
            <div className="mt-1.5 flex gap-2">
              <div className="min-w-0 flex-1">
                <div className="truncate text-[13px] font-semibold text-zinc-900">{title || (empty ? "Your title" : "")}</div>
                <div className="line-clamp-4 break-words text-[12.5px] leading-snug text-zinc-700">{body || (empty ? "Your message" : "")}</div>
              </div>
              {image ? <img src={image} alt="" className="size-10 shrink-0 rounded object-cover" /> : null}
            </div>
            {shown === "ios" && content.ios_badge !== undefined ? <div className="mt-1 text-[10px] text-zinc-500">App badge: {content.ios_badge}</div> : null}
          </div>
        </div>
      )}
      <p className="text-center text-xs text-zinc-500">Each device and browser draws notifications its own way; this is close, not exact.</p>
    </div>
  );
}
