import { useState } from "react";
import { ExternalLink, Plus, Trash2 } from "lucide-react";
import {
  Button,
  Card,
  Checkbox,
  CodeBlock,
  ColorInput,
  CopyField,
  Field,
  IconButton,
  InlineError,
  Input,
  Notice,
  Segmented,
  Select,
  Skeleton,
  Textarea,
  Toggle,
} from "../../components/ui";
import { toast } from "../../lib/toast";
import { useCan } from "../team/api";
import { useHubPreview, useHubSettings, useUpdateHubSettings } from "./api";
import type { HubAvailableType, HubField, HubFont, HubLauncher, HubObjectChoice, HubObjectKind, HubSections, HubSettings, HubSettingsPatch } from "./types";

const MAX_HELP = 20;
const MAX_FIELDS = 20;

/** Settings → Customer hub: the account page shoppers open from the store's site. */
export function HubSettingsPage() {
  const settings = useHubSettings();
  if (settings.isPending) return <Skeleton className="h-96 w-full" />;
  if (settings.error) return <InlineError message={settings.error.message} />;
  return (
    <div className="space-y-4">
      <HubForm key={JSON.stringify(settings.data)} s={settings.data} />
      <PreviewCard />
    </div>
  );
}

type Draft = Omit<HubSettings, "url" | "embed_site" | "link_domain" | "available_types">;

function draftOf(s: HubSettings): Draft {
  return {
    enabled: s.enabled,
    launcher: s.launcher,
    button_label: s.button_label,
    sections: { ...s.sections },
    object_types: s.object_types.map((o) => ({ ...o, fields: [...o.fields] })),
    help: s.help.map((h) => ({ ...h })),
    contact_url: s.contact_url,
    contact_label: s.contact_label,
    accent_color: s.accent_color,
    font: s.font,
  };
}

const SECTIONS: { key: keyof HubSections; label: string; description: string }[] = [
  { key: "orders", label: "Orders", description: "Orders from the last two years, with status and parcel tracking." },
  { key: "bookings", label: "Bookings and subscriptions", description: "The custom object records you choose below." },
  { key: "recommendations", label: "Picked for you", description: "Product picks and their back-in-stock requests." },
  { key: "preferences", label: "Email and texts", description: "Unsubscribe, re-subscribe to email, choose lists, stop texts." },
  { key: "help", label: "Help", description: "Your questions and answers and a contact link." },
];

function HubForm({ s }: { s: HubSettings }) {
  const canEdit = useCan("settings.write");
  const update = useUpdateHubSettings();
  const [d, setD] = useState<Draft>(() => draftOf(s));
  const set = (patch: Partial<Draft>) => setD((x) => ({ ...x, ...patch }));
  const dirty = JSON.stringify(d) !== JSON.stringify(draftOf(s));
  const save = () => {
    const patch: HubSettingsPatch = {
      ...d,
      help: d.help.filter((h) => h.question.trim() || h.answer.trim()),
      contact_url: d.contact_url?.trim() || null,
      contact_label: d.contact_label?.trim() || null,
      accent_color: d.accent_color?.trim() || null,
    };
    update.mutate(patch, { onSuccess: () => toast.success("Customer hub saved") });
  };
  return (
    <fieldset disabled={!canEdit} className="min-w-0 space-y-4">
      <Card title="Customer hub">
        <div className="space-y-4">
          <div className="flex items-start gap-3">
            <Toggle checked={d.enabled} onChange={(v) => set({ enabled: v })} label="Turn on the customer hub" />
            <div className="text-sm">
              <div className="font-medium text-zinc-900">Turn on the customer hub</div>
              <p className="text-zinc-500">
                An account page for your shoppers. They sign in with a code we email them, or from a link in your emails (use <code className="font-mono">{"{{ hub_url }}"}</code>).
              </p>
            </div>
          </div>
          <Field label="The hub's address">
            <div className="flex items-center gap-2">
              <div className="min-w-0 flex-1">
                <CopyField value={s.url} />
              </div>
              {s.enabled ? (
                <a href={s.url} target="_blank" rel="noreferrer" className="inline-flex items-center gap-1 text-sm text-accent-700 hover:underline">
                  Open <ExternalLink className="size-3.5" />
                </a>
              ) : null}
            </div>
          </Field>
          {s.embed_site ? (
            <Notice tone="green" title={`Opens in a panel on ${s.embed_site}`}>
              Your branded link domain ({s.link_domain.domain}) is part of your site, so the hub opens over your pages and shoppers stay signed in.
            </Notice>
          ) : (
            <Notice tone="gray" title="Opens in a new tab">
              {s.link_domain.active
                ? `Your link domain (${s.link_domain.domain}) isn't part of your website's domain, so browsers would block the sign-in inside a panel. The hub opens in a new tab instead.`
                : "To open the hub in a panel on your pages, connect a branded link domain on your own domain (like links.yourbrand.com) in Settings → Sending. Without one, browsers block the sign-in inside a panel, so the hub opens in a new tab."}
            </Notice>
          )}
        </div>
      </Card>

      <Card title="How shoppers open it">
        <div className="space-y-4">
          <Segmented<HubLauncher>
            value={d.launcher}
            onChange={(v) => set({ launcher: v })}
            options={[
              { value: "off", label: "Off" },
              { value: "button", label: "Floating button" },
              { value: "link", label: "Your own link or button" },
            ]}
          />
          <p className="text-sm text-zinc-600">
            {d.launcher === "off"
              ? "No launcher. Link to the hub's address yourself."
              : d.launcher === "button"
                ? "A button in the corner of every page that has the website snippet."
                : "Only from a link or button on your site, like the ones below."}
          </p>
          {d.launcher !== "off" ? (
            <Field label="Button text" className="max-w-xs">
              <Input value={d.button_label} maxLength={40} onChange={(e) => set({ button_label: e.target.value })} />
            </Field>
          ) : null}
          <div>
            <div className="mb-1 text-xs font-medium text-zinc-700">Your own link or button (works with every choice above)</div>
            <CodeBlock code={'<a href="#" data-dumbmail-hub>My account</a>\n<button type="button" onclick="DumbMail.openHub()">My account</button>'} />
          </div>
        </div>
      </Card>

      <Card title="Sections">
        <div className="grid gap-3 sm:grid-cols-2">
          {SECTIONS.map((x) => (
            <Checkbox key={x.key} label={x.label} description={x.description} checked={d.sections[x.key]} onChange={(e) => set({ sections: { ...d.sections, [x.key]: e.target.checked } })} />
          ))}
        </div>
      </Card>

      {d.sections.bookings ? <ObjectsCard types={s.available_types} value={d.object_types} onChange={(v) => set({ object_types: v })} /> : null}

      {d.sections.help ? (
        <Card title="Help">
          <div className="space-y-3">
            {d.help.map((h, i) => (
              <div key={i} className="grid gap-2 rounded-md border border-zinc-200 p-3 sm:grid-cols-[1fr_auto]">
                <div className="space-y-2">
                  <Input value={h.question} maxLength={200} placeholder="e.g. When will my order arrive?" aria-label={`Question ${i + 1}`} onChange={(e) => set({ help: d.help.map((x, j) => (j === i ? { ...x, question: e.target.value } : x)) })} />
                  <Textarea value={h.answer} maxLength={2000} rows={2} placeholder="The answer" aria-label={`Answer ${i + 1}`} onChange={(e) => set({ help: d.help.map((x, j) => (j === i ? { ...x, answer: e.target.value } : x)) })} />
                </div>
                <IconButton label={`Remove question ${i + 1}`} onClick={() => set({ help: d.help.filter((_, j) => j !== i) })}>
                  <Trash2 className="size-4" />
                </IconButton>
              </div>
            ))}
            {d.help.length < MAX_HELP ? (
              <Button size="sm" icon={<Plus className="size-3.5" />} onClick={() => set({ help: [...d.help, { question: "", answer: "" }] })}>
                Add a question
              </Button>
            ) : null}
            <div className="grid gap-3 sm:grid-cols-2">
              <Field label="Contact link" hint="A web page or an email address.">
                <Input value={d.contact_url ?? ""} placeholder="help@yourbrand.com" onChange={(e) => set({ contact_url: e.target.value })} />
              </Field>
              <Field label="Contact button text">
                <Input value={d.contact_label ?? ""} maxLength={40} placeholder="Contact us" onChange={(e) => set({ contact_label: e.target.value })} />
              </Field>
            </div>
          </div>
        </Card>
      ) : null}

      <Card title="Look">
        <div className="grid gap-3 sm:grid-cols-2">
          <Field label="Buttons and links" hint="Your brand colour when empty. The logo comes from your brand settings.">
            <ColorInput value={d.accent_color ?? ""} onChange={(v) => set({ accent_color: v })} aria-label="Accent color" />
          </Field>
          <Field label="Font">
            <Select value={d.font} onChange={(e) => set({ font: e.target.value as HubFont })}>
              <option value="system">System (like the shopper's phone)</option>
              <option value="sans">Clean sans-serif</option>
              <option value="serif">Serif</option>
            </Select>
          </Field>
        </div>
      </Card>

      {canEdit ? (
        <div className="flex items-center justify-end gap-3">
          <InlineError message={update.error?.message} />
          <Button variant="primary" loading={update.isPending} disabled={!dirty} onClick={save}>
            Save
          </Button>
        </div>
      ) : (
        <p className="text-sm text-zinc-500">You can look at these settings. Changing them needs an Admin or Owner.</p>
      )}
    </fieldset>
  );
}

/** Why a field can't be shown, if it can't. */
function blocked(f: HubField, kind: HubObjectKind): string | null {
  if (f.sensitive) return "sensitive";
  if (kind === "booking" && f.access_like) return "access or address";
  return null;
}

function ObjectsCard({ types, value, onChange }: { types: HubAvailableType[]; value: HubObjectChoice[]; onChange: (v: HubObjectChoice[]) => void }) {
  const choice = (key: string) => value.find((c) => c.type === key);
  const setChoice = (key: string, c: HubObjectChoice | null) => {
    const rest = value.filter((x) => x.type !== key);
    onChange(c ? [...rest, c] : rest);
  };
  return (
    <Card title="Bookings and subscriptions">
      {types.length === 0 ? (
        <p className="text-sm text-zinc-500">You don't have any custom object types yet. Add them in Audience → Objects, or connect an integration that records bookings or subscriptions.</p>
      ) : (
        <div className="space-y-3">
          <p className="text-sm text-zinc-600">
            Choose the records shoppers see and the fields shown for each. Only records linked to the shopper show. Sensitive fields never show, and for bookings, neither does anything about access or the address.
          </p>
          {types.map((t) => {
            const c = choice(t.key);
            return (
              <div key={t.key} className="rounded-md border border-zinc-200 p-3">
                <div className="flex flex-wrap items-center justify-between gap-3">
                  <Checkbox
                    label={t.plural}
                    description={`${t.count.toLocaleString()} record${t.count === 1 ? "" : "s"}`}
                    checked={!!c}
                    onChange={(e) => setChoice(t.key, e.target.checked ? { type: t.key, kind: "other", fields: [] } : null)}
                  />
                  {c ? (
                    <Select
                      value={c.kind}
                      className="w-44"
                      aria-label={`What ${t.plural} are`}
                      onChange={(e) => {
                        const kind = e.target.value as HubObjectKind;
                        const fields = c.fields.filter((k) => {
                          const f = t.fields.find((x) => x.key === k);
                          return f ? !blocked(f, kind) : false;
                        });
                        setChoice(t.key, { ...c, kind, fields });
                      }}
                    >
                      <option value="booking">Bookings</option>
                      <option value="subscription">Subscriptions</option>
                      <option value="other">Other records</option>
                    </Select>
                  ) : null}
                </div>
                {c ? (
                  <div className="mt-3 grid gap-2 sm:grid-cols-2">
                    {t.fields.map((f) => {
                      const why = blocked(f, c.kind);
                      const on = c.fields.includes(f.key);
                      const full = !on && c.fields.length >= MAX_FIELDS;
                      return (
                        <Checkbox
                          key={f.key}
                          label={f.label || f.key}
                          description={why ? `Can't be shown (${why})` : f.label ? f.key : undefined}
                          checked={on && !why}
                          disabled={!!why || full}
                          onChange={(e) => setChoice(t.key, { ...c, fields: e.target.checked ? [...c.fields, f.key] : c.fields.filter((k) => k !== f.key) })}
                        />
                      );
                    })}
                  </div>
                ) : null}
              </div>
            );
          })}
        </div>
      )}
    </Card>
  );
}

function PreviewCard() {
  const preview = useHubPreview();
  return (
    <Card title="Preview" actions={<span className="text-xs text-zinc-500">A made-up shopper, with your saved settings</span>}>
      {preview.isPending ? (
        <Skeleton className="h-[560px] w-full" />
      ) : preview.error ? (
        <InlineError message={preview.error.message} />
      ) : (
        <iframe title="Customer hub preview" srcDoc={preview.data.html} sandbox="" className="block h-[560px] w-full rounded-md border border-zinc-200 bg-zinc-50" />
      )}
    </Card>
  );
}
