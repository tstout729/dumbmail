import { useState, type FormEvent } from "react";
import { AlertTriangle, CheckCircle2, Plus, RefreshCw, Trash2 } from "lucide-react";
import {
  Badge,
  Button,
  Card,
  ConfirmDialog,
  CopyField,
  EmptyRow,
  Field,
  IconButton,
  InlineError,
  Input,
  Modal,
  Notice,
  Select,
  Skeleton,
  Stat,
  Table,
  TagInput,
  Td,
  Textarea,
  Th,
  Checkbox,
} from "../../components/ui";
import { formatPhone, formatRelative } from "../../lib/format";
import { toast } from "../../lib/toast";
import { useCan } from "../team/api";
import {
  useAddTestNumber,
  useRemoveTestNumber,
  useCheckWhatsapp,
  useCreateTemplate,
  useDisconnectWhatsapp,
  useRotateVerifyToken,
  useSyncTemplates,
  useUpdateWhatsappSettings,
  useWhatsappSettings,
  useWhatsappTemplates,
} from "./api";
import { categoryLabel, statusTone } from "./WhatsappEditor";
import type { NewTemplate, NewTemplateButton, WhatsappSettings } from "./types";

/** Settings → WhatsApp: the connection to your own WhatsApp Business account, its webhook, test numbers and templates. */
export function WhatsappSettingsPage() {
  const settings = useWhatsappSettings();
  if (settings.isPending) return <Skeleton className="h-64 w-full" />;
  if (settings.error) return <InlineError message={settings.error.message} />;
  const s = settings.data;
  return (
    <div className="space-y-4">
      <div className="grid grid-cols-2 gap-3 md:grid-cols-4">
        <Stat label="Sending" value={s.connected ? "On" : "Off"} sub={s.phone.verified_name ?? undefined} />
        <Stat label="Quality rating" value={s.phone.quality_rating ? s.phone.quality_rating.toLowerCase() : "—"} sub="From Meta" />
        <Stat label="Messaging limit" value={limitLabel(s.phone.messaging_limit)} sub="People a day, outside conversations" />
        <Stat label="Approved templates" value={`${s.templates.approved} of ${s.templates.total}`} />
      </div>
      {s.missing.length > 0 && s.missing.length < 4 ? (
        <Notice tone="amber" icon={<AlertTriangle />} title="Sending isn't on yet">
          Still needed: {s.missing.join(", ")}. The app secret is required so DumbMail can check Meta's webhooks, including opt-outs.
        </Notice>
      ) : null}
      {s.error ? (
        <Notice tone="red" icon={<AlertTriangle />} title="Meta reported a problem">
          {s.error}
        </Notice>
      ) : null}
      <Connection settings={s} />
      <Webhook settings={s} />
      <Signup key={`${s.signup_keywords.join(",")}|${s.optin_buttons.join(",")}|${s.signup_reply ?? ""}`} settings={s} />
      <TestNumbers settings={s} />
      <Templates connected={s.connected} />
    </div>
  );
}

function limitLabel(l: string | null): string {
  if (!l) return "—";
  const m = /^TIER_(.+)$/.exec(l);
  if (!m) return l.toLowerCase();
  return m[1] === "UNLIMITED" ? "Unlimited" : m[1].replace("K", ",000");
}

function Connection({ settings: s }: { settings: WhatsappSettings }) {
  const canWrite = useCan("settings.write");
  const update = useUpdateWhatsappSettings();
  const check = useCheckWhatsapp();
  const disconnect = useDisconnectWhatsapp();
  const [waba, setWaba] = useState(s.waba_id ?? "");
  const [phone, setPhone] = useState(s.phone_number_id ?? "");
  const [token, setToken] = useState("");
  const [secret, setSecret] = useState("");
  const [confirming, setConfirming] = useState(false);
  const submit = (e: FormEvent) => {
    e.preventDefault();
    update.mutate(
      { waba_id: waba.trim() || undefined, phone_number_id: phone.trim() || undefined, access_token: token.trim() || undefined, app_secret: secret.trim() || undefined },
      {
        onSuccess: () => {
          setToken("");
          setSecret("");
          toast.success("Saved");
        },
      },
    );
  };
  return (
    <Card
      title="Your WhatsApp Business account"
      actions={s.connected ? <Badge tone="green">Connected</Badge> : <Badge>Not connected</Badge>}
    >
      <div className="space-y-4 text-sm text-zinc-600">
        <p>
          DumbMail sends through Meta's WhatsApp Cloud API with your own WhatsApp Business account. Meta bills your business for WhatsApp conversations as
          usual.
        </p>
        <ol className="list-decimal space-y-1.5 pl-5 text-xs">
          <li>In Meta Business Settings, add a system user with access to your WhatsApp Business Account and your Meta app.</li>
          <li>
            Generate a permanent token for the app with the <code>whatsapp_business_messaging</code> and <code>whatsapp_business_management</code> permissions.
          </li>
          <li>In WhatsApp Manager, copy the WhatsApp Business Account ID and the phone number ID.</li>
          <li>In your Meta app, copy the app secret (App settings → Basic). It checks that webhooks really come from Meta.</li>
        </ol>
        <form onSubmit={submit} className="grid grid-cols-1 gap-3 md:grid-cols-2">
          <Field label="WhatsApp Business Account ID">
            <Input value={waba} onChange={(e) => setWaba(e.target.value)} inputMode="numeric" disabled={!canWrite} placeholder="102290129340398" />
          </Field>
          <Field label="Phone number ID">
            <Input value={phone} onChange={(e) => setPhone(e.target.value)} inputMode="numeric" disabled={!canWrite} placeholder="106540352242922" />
          </Field>
          <Field label="Access token" hint={s.access_token_set ? `Saved (${s.access_token_hint ?? "hidden"}). Paste a new one to replace it.` : "Shown only once; it's encrypted."}>
            <Input type="password" autoComplete="off" value={token} onChange={(e) => setToken(e.target.value)} disabled={!canWrite} placeholder={s.access_token_set ? "••••••••" : "EAAG…"} />
          </Field>
          <Field label="App secret" hint={s.app_secret_set ? "Saved. Paste a new one to replace it." : "Needed before webhooks are accepted."}>
            <Input type="password" autoComplete="off" value={secret} onChange={(e) => setSecret(e.target.value)} disabled={!canWrite} placeholder={s.app_secret_set ? "••••••••" : ""} />
          </Field>
          {canWrite ? (
            <div className="flex flex-wrap gap-2 md:col-span-2">
              <Button type="submit" variant="primary" loading={update.isPending}>
                Save
              </Button>
              {s.connected ? (
                <Button type="button" variant="danger" icon={<Trash2 className="size-3.5" />} onClick={() => setConfirming(true)}>
                  Disconnect
                </Button>
              ) : null}
            </div>
          ) : (
            <p className="text-xs text-zinc-500 md:col-span-2">Only owners and admins can change the connection.</p>
          )}
        </form>
        {update.error ? <InlineError message={update.error.message} /> : null}
        {s.connected ? (
          <div className="flex flex-wrap items-center gap-3 border-t border-zinc-100 pt-3">
            <Button variant="secondary" icon={<RefreshCw className="size-3.5" />} loading={check.isPending} onClick={() => check.mutate(undefined, { onSuccess: (r) => (r.error ? toast.error(r.error) : toast.success("Connection works")) })}>
              Check connection
            </Button>
            {s.phone.checked_at ? (
              <span className="text-xs text-zinc-500">
                {s.phone.verified_name ?? "Unnamed"} · {formatPhone(s.phone.display_phone_number ?? "") || s.phone.display_phone_number} · checked {formatRelative(s.phone.checked_at)}
              </span>
            ) : (
              <span className="text-xs text-zinc-500">Not checked yet.</span>
            )}
          </div>
        ) : null}
        {check.error ? <InlineError message={check.error.message} /> : null}
      </div>
      {confirming ? (
        <ConfirmDialog
          title="Disconnect WhatsApp?"
          body="DumbMail forgets the token and app secret. WhatsApp campaigns, flow steps and replies stop until you connect again. Templates stay."
          confirmLabel="Disconnect"
          danger
          loading={disconnect.isPending}
          onConfirm={() => disconnect.mutate(undefined, { onSuccess: () => setConfirming(false) })}
          onClose={() => setConfirming(false)}
        />
      ) : null}
    </Card>
  );
}

const FIELDS = ["messages", "message_template_status_update", "message_template_quality_update", "phone_number_quality_update", "user_preferences"];

function Webhook({ settings: s }: { settings: WhatsappSettings }) {
  const canWrite = useCan("settings.write");
  const rotate = useRotateVerifyToken();
  return (
    <Card
      title="Webhook"
      actions={s.webhook_working ? <Badge tone="green">Receiving</Badge> : <Badge>Nothing received yet</Badge>}
    >
      <div className="space-y-3 text-sm text-zinc-600">
        <p>
          Meta tells DumbMail about replies, delivery and read receipts, template reviews and opt-outs through a webhook. In your Meta app, open WhatsApp →
          Configuration, paste these two values, click Verify and save, then subscribe to the fields below.
        </p>
        <Field label="Callback URL">
          <CopyField value={s.webhook_url} />
        </Field>
        <Field label="Verify token">
          <div className="flex items-center gap-2">
            <div className="min-w-0 flex-1">
              <CopyField value={s.verify_token} />
            </div>
            {canWrite ? (
              <Button size="sm" variant="ghost" loading={rotate.isPending} onClick={() => rotate.mutate(undefined, { onSuccess: () => toast.success("New verify token made") })}>
                New token
              </Button>
            ) : null}
          </div>
        </Field>
        <div>
          <div className="text-xs font-medium text-zinc-700">Webhook fields to subscribe to</div>
          <div className="mt-1 flex flex-wrap gap-1.5">
            {FIELDS.map((f) => (
              <code key={f} className="rounded bg-zinc-100 px-1.5 py-0.5 text-xs">
                {f}
              </code>
            ))}
          </div>
        </div>
        {s.last_webhook_at ? (
          <p className="flex items-center gap-1.5 text-xs text-zinc-500">
            <CheckCircle2 className="size-3.5 text-accent-700" /> Last delivery from Meta {formatRelative(s.last_webhook_at)}.
          </p>
        ) : null}
        {!s.app_secret_set ? <p className="text-xs text-amber-700">Save your app secret first: webhooks without a valid signature are refused.</p> : null}
      </div>
    </Card>
  );
}

function TestNumbers({ settings: s }: { settings: WhatsappSettings }) {
  const canTest = useCan("campaigns.test");
  const add = useAddTestNumber();
  const remove = useRemoveTestNumber();
  const [phone, setPhone] = useState("");
  const [pending, setPending] = useState<{ code: string; phone_number: string } | null>(null);
  return (
    <Card title="Test numbers">
      <div className="space-y-3 text-sm text-zinc-600">
        <p>
          WhatsApp tests only go to team members' own phones. Add yours, then send the code shown from that phone, on WhatsApp, to your business number. Tests stop
          going to a number when its member leaves the team.
        </p>
        {s.test_numbers.length ? (
          <ul className="divide-y divide-zinc-100 rounded-md border border-zinc-200">
            {s.test_numbers.map((t) => (
              <li key={t.id} className="flex flex-wrap items-center justify-between gap-2 px-3 py-2">
                <span className="tabular-nums text-zinc-800">{formatPhone(t.phone_number)}</span>
                <span className="text-xs text-zinc-500">{t.member}</span>
                {t.verified ? <Badge tone="green">Verified</Badge> : <Badge tone="amber">Waiting for the code</Badge>}
                <IconButton label="Remove test number" onClick={() => remove.mutate(t.id)}>
                  <Trash2 className="size-4" />
                </IconButton>
              </li>
            ))}
          </ul>
        ) : null}
        {canTest ? (
          <form
            className="flex flex-wrap items-end gap-2"
            onSubmit={(e: FormEvent) => {
              e.preventDefault();
              add.mutate(phone, {
                onSuccess: (r) => {
                  setPending(r);
                  setPhone("");
                },
              });
            }}
          >
            <Field label="Your phone">
              <Input value={phone} onChange={(e) => setPhone(e.target.value)} placeholder="+15035550100" className="w-48" inputMode="tel" />
            </Field>
            <Button type="submit" variant="secondary" loading={add.isPending} disabled={!phone.trim()}>
              Add my number
            </Button>
          </form>
        ) : null}
        {pending ? (
          <Notice tone="blue" title={`Send ${pending.code} from ${formatPhone(pending.phone_number)}`}>
            Open WhatsApp on that phone and send exactly <b>{pending.code}</b> to your business number{s.phone.display_phone_number ? ` (${s.phone.display_phone_number})` : ""} within 30 minutes.
          </Notice>
        ) : null}
        {add.error ? <InlineError message={add.error.message} /> : null}
      </div>
    </Card>
  );
}

function Signup({ settings: s }: { settings: WhatsappSettings }) {
  const canWrite = useCan("settings.write");
  const update = useUpdateWhatsappSettings();
  const [words, setWords] = useState(s.signup_keywords);
  const [buttons, setButtons] = useState(s.optin_buttons);
  const [reply, setReply] = useState(s.signup_reply ?? "");
  const changed = words.join(",") !== s.signup_keywords.join(",") || buttons.join(",") !== s.optin_buttons.join(",") || reply !== (s.signup_reply ?? "");
  return (
    <Card title="Sign-ups on WhatsApp">
      <div className="space-y-3 text-sm text-zinc-600">
        <p>
          Someone is subscribed to WhatsApp marketing only when they ask for it on WhatsApp: by sending exactly one of these words, or tapping a quick-reply
          button with one of these payloads. Texting keywords don't sign anyone up here.
        </p>
        <Field label="Sign-up words" hint="One word each, like JOIN.">
          <TagInput value={words} onChange={setWords} placeholder="JOIN" aria-label="Sign-up words" />
        </Field>
        <Field label="Opt-in button payloads" hint="The payload of a quick-reply button in your templates that means yes.">
          <TagInput value={buttons} onChange={setButtons} placeholder="YES_WHATSAPP" aria-label="Opt-in button payloads" />
        </Field>
        <Field label="Answer" hint={'"Reply STOP to unsubscribe." is added.'}>
          <Textarea rows={2} value={reply} onChange={(e) => setReply(e.target.value)} placeholder="Thanks! You're subscribed to WhatsApp messages from us." maxLength={1000} />
        </Field>
        {canWrite ? (
          <Button
            variant="secondary"
            disabled={!changed}
            loading={update.isPending}
            onClick={() => update.mutate({ signup_keywords: words, optin_buttons: buttons, signup_reply: reply }, { onSuccess: () => toast.success("Sign-up settings saved") })}
          >
            Save
          </Button>
        ) : null}
        {update.error ? <InlineError message={update.error.message} /> : null}
      </div>
    </Card>
  );
}

function Templates({ connected }: { connected: boolean }) {
  const canWrite = useCan("settings.write");
  const templates = useWhatsappTemplates();
  const sync = useSyncTemplates();
  const [creating, setCreating] = useState(false);
  const list = templates.data?.templates ?? [];
  return (
    <Card
      title="Templates"
      padded={false}
      actions={
        connected ? (
          <div className="flex gap-2">
            <Button size="sm" variant="secondary" icon={<RefreshCw className="size-3.5" />} loading={sync.isPending} onClick={() => sync.mutate(undefined, { onSuccess: (r) => toast.success(`Read ${r.read} templates from Meta`) })}>
              Sync
            </Button>
            {canWrite ? (
              <Button size="sm" variant="primary" icon={<Plus className="size-3.5" />} onClick={() => setCreating(true)}>
                New template
              </Button>
            ) : null}
          </div>
        ) : null
      }
    >
      <p className="border-b border-zinc-100 px-4 py-2.5 text-xs text-zinc-500">
        Outside the 24 hours after someone writes to you, WhatsApp only allows templates Meta approved. Meta's reviews and pauses update here on their own.
      </p>
      {sync.error ? (
        <div className="px-4 py-2">
          <InlineError message={sync.error.message} />
        </div>
      ) : null}
      <div className="overflow-x-auto">
        <Table className="rounded-none border-0 shadow-none">
          <thead>
            <tr>
              <Th>Name</Th>
              <Th>Language</Th>
              <Th>Category</Th>
              <Th>Status</Th>
              <Th className="hidden md:table-cell">Message</Th>
            </tr>
          </thead>
          <tbody>
            {templates.isPending ? (
              <tr>
                <Td colSpan={5}>
                  <Skeleton className="h-6 w-full" />
                </Td>
              </tr>
            ) : list.length === 0 ? (
              <EmptyRow cols={5}>
                <p className="px-4 py-6 text-center text-sm text-zinc-500">{connected ? "No templates yet. Sync them from Meta or make one." : "Connect WhatsApp to see your templates."}</p>
              </EmptyRow>
            ) : (
              list.map((t) => (
                <tr key={t.id}>
                  <Td className="font-medium text-zinc-900">{t.name}</Td>
                  <Td>{t.language}</Td>
                  <Td>{categoryLabel(t.category)}</Td>
                  <Td>
                    <span title={t.reason ?? undefined}>
                      <Badge tone={statusTone(t.status)}>{t.status.toLowerCase()}</Badge>
                    </span>
                  </Td>
                  <Td className="hidden max-w-md truncate text-zinc-500 md:table-cell">{t.components.find((c) => c.type.toUpperCase() === "BODY")?.text}</Td>
                </tr>
              ))
            )}
          </tbody>
        </Table>
      </div>
      {creating ? <CreateTemplate onClose={() => setCreating(false)} /> : null}
    </Card>
  );
}

function bodyVars(body: string): number {
  return new Set([...body.matchAll(/\{\{\s*(\d+)\s*\}\}/g)].map((m) => m[1])).size;
}

function CreateTemplate({ onClose }: { onClose: () => void }) {
  const create = useCreateTemplate();
  const [t, setT] = useState<NewTemplate>({ name: "", language: "en_US", category: "UTILITY", header: "", body: "", body_examples: [], footer: "", buttons: [] });
  const set = (patch: Partial<NewTemplate>) => setT({ ...t, ...patch });
  const vars = bodyVars(t.body);
  const examples = Array.from({ length: vars }, (_, i) => t.body_examples[i] ?? "");
  const headerVar = /\{\{\s*1\s*\}\}/.test(t.header ?? "");
  const setButton = (i: number, patch: Partial<NewTemplateButton>) => set({ buttons: t.buttons.map((b, j) => (j === i ? { ...b, ...patch } : b)) });
  const submit = () =>
    create.mutate(
      { ...t, header: t.header?.trim() || undefined, footer: t.footer?.trim() || undefined, header_example: headerVar ? t.header_example : undefined, body_examples: examples },
      {
        onSuccess: () => {
          toast.success("Sent to Meta for review");
          onClose();
        },
      },
    );
  return (
    <Modal
      title="New WhatsApp template"
      description="Meta reviews every template, usually within minutes and at most a day. You can use it once it's approved."
      onClose={onClose}
      size="lg"
      footer={
        <>
          <Button variant="secondary" onClick={onClose}>
            Cancel
          </Button>
          <Button variant="primary" loading={create.isPending} onClick={submit} disabled={!t.name || !t.body}>
            Submit for review
          </Button>
        </>
      }
    >
      <div className="space-y-3">
        <div className="grid grid-cols-1 gap-3 sm:grid-cols-3">
          <Field label="Name" hint="Lowercase letters, digits and _">
            <Input value={t.name} onChange={(e) => set({ name: e.target.value.toLowerCase().replace(/[^a-z0-9_]/g, "_") })} placeholder="order_update" />
          </Field>
          <Field label="Language">
            <Input value={t.language} onChange={(e) => set({ language: e.target.value })} placeholder="en_US" />
          </Field>
          <Field label="Category">
            <Select value={t.category} onChange={(e) => set({ category: e.target.value as NewTemplate["category"] })}>
              <option value="UTILITY">Utility (orders, bookings)</option>
              <option value="MARKETING">Marketing (promotions)</option>
            </Select>
          </Field>
        </div>
        <Field label="Header (optional)" hint="Up to 60 characters, with at most one variable {{1}}.">
          <Input value={t.header ?? ""} maxLength={60} onChange={(e) => set({ header: e.target.value })} placeholder="Your order {{1}}" />
        </Field>
        {headerVar ? (
          <Field label="Header example">
            <Input value={t.header_example ?? ""} onChange={(e) => set({ header_example: e.target.value })} placeholder="A-1001" />
          </Field>
        ) : null}
        <Field label="Message" hint="Variables are {{1}}, {{2}} … in order. Meta doesn't allow one at the very start or end.">
          <Textarea rows={4} value={t.body} maxLength={1024} onChange={(e) => set({ body: e.target.value })} placeholder="Hi {{1}}, your order {{2}} is on its way." />
        </Field>
        {examples.map((ex, i) => (
          <Field key={i} label={`Example for {{${i + 1}}}`}>
            <Input value={ex} onChange={(e) => set({ body_examples: examples.map((x, j) => (j === i ? e.target.value : x)) })} />
          </Field>
        ))}
        <Field label="Footer (optional)" hint="Up to 60 characters, no variables.">
          <Input value={t.footer ?? ""} maxLength={60} onChange={(e) => set({ footer: e.target.value })} placeholder="Reply STOP to unsubscribe" />
        </Field>
        <div className="space-y-2">
          <div className="text-xs font-medium text-zinc-700">Buttons (optional)</div>
          {t.buttons.map((b, i) => (
            <div key={i} className="flex flex-col gap-2 rounded-md border border-zinc-200 p-2 sm:flex-row sm:items-center">
              <Select value={b.type} onChange={(e) => setButton(i, { type: e.target.value as NewTemplateButton["type"] })} className="sm:w-36" aria-label={`Button ${i + 1} type`}>
                <option value="quick_reply">Quick reply</option>
                <option value="url">Link</option>
              </Select>
              <Input value={b.text} maxLength={25} onChange={(e) => setButton(i, { text: e.target.value })} placeholder="Label" aria-label={`Button ${i + 1} label`} className="sm:w-40" />
              {b.type === "url" ? (
                <>
                  <Input value={b.url ?? ""} onChange={(e) => setButton(i, { url: e.target.value })} placeholder="https://…" aria-label={`Button ${i + 1} link`} />
                  <Checkbox label="Track clicks" checked={b.track !== false} onChange={(e) => setButton(i, { track: e.target.checked })} />
                </>
              ) : null}
              <IconButton label="Remove button" onClick={() => set({ buttons: t.buttons.filter((_, j) => j !== i) })}>
                <Trash2 className="size-4" />
              </IconButton>
            </div>
          ))}
          {t.buttons.length < 10 ? (
            <Button size="sm" variant="ghost" icon={<Plus className="size-3.5" />} onClick={() => set({ buttons: [...t.buttons, { type: "quick_reply", text: "" }] })}>
              Add button
            </Button>
          ) : null}
          <p className="text-xs text-zinc-500">
            Tracked links count clicks in reports and attribution. Make templates with an image, video or document header in WhatsApp Manager, then sync.
          </p>
        </div>
        {create.error ? <InlineError message={create.error.message} /> : null}
      </div>
    </Modal>
  );
}
