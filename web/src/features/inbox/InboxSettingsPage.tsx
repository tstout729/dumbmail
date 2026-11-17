import { useState } from "react";
import { Pencil, Plus, Trash2 } from "lucide-react";
import { errorMessage, useSession } from "../../api";
import { Button, Card, Checkbox, ConfirmDialog, EmptyState, Field, IconButton, InlineError, Input, Modal, Notice, Segmented, Select, Skeleton, Textarea, Toggle } from "../../components/ui";
import { toast } from "../../lib/toast";
import { useDeleteSavedReply, useInboxSettings, useSaveInboxSettings, useSavedReplies, useSaveSavedReply } from "./api";
import type { HelpReplies, InboxSettings, InboxSettingsPatch, SavedReply } from "./types";

const DAYS = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];

function hourLabel(h: number): string {
  if (h === 0 || h === 24) return "12 am";
  if (h === 12) return "12 pm";
  return h < 12 ? `${h} am` : `${h - 12} pm`;
}

export function InboxSettingsPage() {
  const settings = useInboxSettings();
  if (settings.isPending) return <Skeleton className="h-96" />;
  if (settings.error) return <p className="text-sm text-red-600">{settings.error.message}</p>;
  return (
    <div className="space-y-4">
      <SettingsForm key={JSON.stringify(settings.data)} initial={settings.data} />
      <SavedReplies />
    </div>
  );
}

function SettingsForm({ initial }: { initial: InboxSettings }) {
  const { account } = useSession();
  const save = useSaveInboxSettings();
  const [s, setS] = useState(initial);
  const [telnyxKey, setTelnyxKey] = useState("");
  const set = (patch: Partial<InboxSettings>) => setS((v) => ({ ...v, ...patch }));
  const submit = () => {
    const patch: InboxSettingsPatch = {
      program_name: s.program_name,
      message_frequency: s.message_frequency,
      support_contact: s.support_contact,
      help_replies: s.help_replies,
      auto_budget_account: s.auto_budget_account,
      auto_budget_number: s.auto_budget_number,
      default_reply_enabled: s.default_reply_enabled,
      default_reply: s.default_reply,
      away_reply_enabled: s.away_reply_enabled,
      away_reply: s.away_reply,
      open_hour: s.open_hour,
      close_hour: s.close_hour,
      open_days: s.open_days,
      notify_user_ids: s.notify_user_ids,
      ...(telnyxKey.trim() ? { telnyx_public_key: telnyxKey.trim() } : {}),
    };
    save.mutate(patch, { onSuccess: () => toast.success("Inbox settings saved") });
  };
  const toggleDay = (d: number) => set({ open_days: s.open_days.includes(d) ? s.open_days.filter((x) => x !== d) : [...s.open_days, d].sort() });
  const toggleMember = (id: string) => set({ notify_user_ids: s.notify_user_ids.includes(id) ? s.notify_user_ids.filter((x) => x !== id) : [...s.notify_user_ids, id] });

  const v = s.verification;
  return (
    <>
      <Card title="Checking texts come from your provider">
        {v.configured ? (
          <Notice tone="green" title="Inbound texts are verified">
            Each text's signature from {v.provider === "twilio" ? "Twilio" : "Telnyx"} is checked, and forged ones are refused.
          </Notice>
        ) : (
          <Notice tone="amber" title="Keywords, automatic replies and reply windows need your provider's signing key">
            Without it, texts are still kept and STOP is always honored, but a text can't sign anyone up, get an automatic reply, open the 24-hour reply window or start a
            flow, because anyone who learned your webhook address could fake one. {v.how}
          </Notice>
        )}
        {v.provider === "telnyx" && (s.telnyx_public_key_hint !== null || !v.configured) ? (
          <div className="mt-3">
            <Field
              label="Telnyx public key (your own Telnyx account only)"
              hint={s.telnyx_public_key_hint ? `Saved: ${s.telnyx_public_key_hint} Paste a new one to replace it.` : "Telnyx portal → Keys & Credentials → Public Key."}
            >
              <Input value={telnyxKey} onChange={(e) => setTelnyxKey(e.target.value)} placeholder="Base64 key" className="font-mono" autoComplete="off" />
            </Field>
          </div>
        ) : null}
      </Card>
      <Card title="Sign-up texts">
        <p className="mb-3 text-sm text-zinc-600">
          When someone texts a sign-up keyword like JOIN, the confirmation must say who you are and how often you text. DumbMail adds "Msg & data rates may apply. Reply
          HELP for help, STOP to cancel." for you.
        </p>
        <div className="grid grid-cols-1 gap-4 sm:grid-cols-2">
          <Field label="Program name" hint="Starts every sign-up text.">
            <Input value={s.program_name} onChange={(e) => set({ program_name: e.target.value })} maxLength={60} placeholder={`${account.name} Texts`} />
          </Field>
          <Field label="Message frequency">
            <Input value={s.message_frequency} onChange={(e) => set({ message_frequency: e.target.value })} maxLength={60} placeholder="Up to 4 msgs/month" />
          </Field>
        </div>
      </Card>
      <Card title="HELP">
        <p className="mb-3 text-sm text-zinc-600">
          Anyone who texts HELP or INFO gets one fixed answer (at most once an hour): "{s.program_name || account.name}: For help, contact {s.support_contact || "…"}. Msg &
          data rates may apply. Reply STOP to cancel." Keywords can't be turned on without a support contact.
        </p>
        <div className="space-y-4">
          <Field label="Support contact" hint="An email address or phone number.">
            <Input value={s.support_contact} onChange={(e) => set({ support_contact: e.target.value })} maxLength={80} placeholder={`help@${account.name.toLowerCase().replace(/[^a-z0-9]/g, "") || "example"}.com`} />
          </Field>
          <Field
            label="Who answers HELP"
            hint="Automatic follows your provider: Twilio answers HELP itself on regular and toll-free numbers, so DumbMail doesn't send a second answer; on Telnyx, DumbMail answers."
          >
            <Segmented<HelpReplies>
              value={s.help_replies}
              onChange={(h) => set({ help_replies: h })}
              options={[
                { value: "auto", label: "Automatic" },
                { value: "dumbmail", label: "Always DumbMail" },
                { value: "provider", label: "Only my provider" },
              ]}
            />
          </Field>
        </div>
      </Card>
      <Card title="Automatic replies">
        <div className="space-y-4">
          <ReplySetting
            title="Default reply"
            description="Answers texts that don't match a keyword, within 24 hours of their text. Each person gets it at most once every 12 hours, so two automated numbers can't text each other forever."
            enabled={s.default_reply_enabled}
            text={s.default_reply}
            placeholder="Thanks for your text! We'll get back to you soon."
            onEnabled={(v) => set({ default_reply_enabled: v })}
            onText={(v) => set({ default_reply: v })}
          />
          <ReplySetting
            title="Away reply"
            description="Sent instead of the default reply outside your opening hours."
            enabled={s.away_reply_enabled}
            text={s.away_reply}
            placeholder="We're closed right now. We'll text you back when we open."
            onEnabled={(v) => set({ away_reply_enabled: v })}
            onText={(v) => set({ away_reply: v })}
          />
          <div>
            <div className="mb-1.5 text-xs font-medium text-zinc-700">Opening hours ({account.timezone})</div>
            <div className="flex flex-wrap items-center gap-2">
              {DAYS.map((d, i) => (
                <button
                  key={d}
                  type="button"
                  aria-pressed={s.open_days.includes(i + 1)}
                  onClick={() => toggleDay(i + 1)}
                  className={
                    s.open_days.includes(i + 1)
                      ? "h-7 rounded-md bg-accent-700 px-2.5 text-xs font-medium text-white"
                      : "h-7 rounded-md border border-zinc-300 bg-white px-2.5 text-xs font-medium text-zinc-600 hover:border-zinc-400"
                  }
                >
                  {d}
                </button>
              ))}
              <span className="ml-1 flex items-center gap-1.5 text-sm text-zinc-600">
                <Select value={s.open_hour} onChange={(e) => set({ open_hour: Number(e.target.value) })} className="w-24" aria-label="Opens at">
                  {Array.from({ length: 24 }, (_, h) => (
                    <option key={h} value={h}>
                      {hourLabel(h)}
                    </option>
                  ))}
                </Select>
                to
                <Select value={s.close_hour} onChange={(e) => set({ close_hour: Number(e.target.value) })} className="w-24" aria-label="Closes at">
                  {Array.from({ length: 24 }, (_, h) => h + 1).map((h) => (
                    <option key={h} value={h}>
                      {hourLabel(h)}
                    </option>
                  ))}
                </Select>
              </span>
            </div>
          </div>
        </div>
      </Card>
      <Card title="Limits on automatic texts">
        <p className="mb-3 text-sm text-zinc-600">
          Keyword answers, sign-up texts, automatic replies and HELP answers are billed like any text. These hourly limits stop someone texting from many numbers from
          running up your balance. When one is reached, automatic texts pause until the next hour and the inbox says so. Each person also gets at most 5 an hour.
        </p>
        {s.auto_paused ? (
          <div className="mb-3">
            <Notice tone="amber" title="A limit was reached this hour">
              Automatic texts are paused until the next hour.
            </Notice>
          </div>
        ) : null}
        <div className="grid grid-cols-1 gap-4 sm:grid-cols-2">
          <Field label="Per hour for the account" hint={`Up to ${s.max_budget_account}.`}>
            <Input type="number" min={0} max={s.max_budget_account} value={s.auto_budget_account} onChange={(e) => set({ auto_budget_account: Number(e.target.value) })} />
          </Field>
          <Field label="Per hour for each of your numbers" hint={`Up to ${s.max_budget_number}.`}>
            <Input type="number" min={0} max={s.max_budget_number} value={s.auto_budget_number} onChange={(e) => set({ auto_budget_number: Number(e.target.value) })} />
          </Field>
        </div>
      </Card>
      <Card title="Email me about new texts">
        <p className="mb-3 text-sm text-zinc-600">Chosen members get one email listing who texted, at most every 30 minutes, instead of one email per text.</p>
        <div className="space-y-2">
          {s.members.map((m) => (
            <Checkbox key={m.id} label={m.name ? `${m.name} (${m.email})` : m.email} checked={s.notify_user_ids.includes(m.id)} onChange={() => toggleMember(m.id)} />
          ))}
        </div>
      </Card>
      <InlineError message={save.error ? errorMessage(save.error) : null} />
      <div className="flex justify-end">
        <Button variant="primary" loading={save.isPending} onClick={submit}>
          Save
        </Button>
      </div>
    </>
  );
}

type ReplySettingProps = {
  title: string;
  description: string;
  enabled: boolean;
  text: string;
  placeholder: string;
  onEnabled: (v: boolean) => void;
  onText: (v: string) => void;
};

function ReplySetting({ title, description, enabled, text, placeholder, onEnabled, onText }: ReplySettingProps) {
  return (
    <div className="space-y-2">
      <div className="flex items-start justify-between gap-4">
        <div className="min-w-0">
          <div className="text-sm font-medium text-zinc-900">{title}</div>
          <p className="text-xs text-zinc-500">{description}</p>
        </div>
        <Toggle checked={enabled} onChange={onEnabled} label={title} />
      </div>
      {enabled ? <Textarea value={text} onChange={(e) => onText(e.target.value)} rows={2} maxLength={480} placeholder={placeholder} aria-label={title} /> : null}
    </div>
  );
}

function SavedReplies() {
  const saved = useSavedReplies();
  const [editing, setEditing] = useState<SavedReply | "new" | null>(null);
  const [deleting, setDeleting] = useState<SavedReply | null>(null);
  const remove = useDeleteSavedReply();
  return (
    <Card
      title="Saved replies"
      actions={
        <Button size="sm" icon={<Plus className="size-3.5" />} onClick={() => setEditing("new")}>
          New saved reply
        </Button>
      }
      padded={false}
    >
      {saved.isPending ? (
        <div className="p-4">
          <Skeleton className="h-12" />
        </div>
      ) : saved.error ? (
        <p className="p-4 text-sm text-red-600">{saved.error.message}</p>
      ) : saved.data.length === 0 ? (
        <EmptyState title="No saved replies yet" description="Save answers you send often, like your opening hours or return policy, and insert them in one click." />
      ) : (
        <ul className="divide-y divide-zinc-100">
          {saved.data.map((r) => (
            <li key={r.id} className="flex items-start gap-3 px-4 py-3">
              <div className="min-w-0 flex-1">
                <div className="text-sm font-medium text-zinc-900">{r.name}</div>
                <p className="line-clamp-2 text-xs text-zinc-500">{r.body}</p>
              </div>
              <IconButton label={`Edit ${r.name}`} onClick={() => setEditing(r)}>
                <Pencil className="size-3.5" />
              </IconButton>
              <IconButton label={`Delete ${r.name}`} onClick={() => setDeleting(r)}>
                <Trash2 className="size-3.5" />
              </IconButton>
            </li>
          ))}
        </ul>
      )}
      {editing ? <SavedReplyDialog key={editing === "new" ? "new" : editing.id} reply={editing === "new" ? null : editing} onClose={() => setEditing(null)} /> : null}
      {deleting ? (
        <ConfirmDialog
          title={`Delete "${deleting.name}"?`}
          body="This only removes the saved reply; texts already sent stay in their conversations."
          confirmLabel="Delete"
          danger
          loading={remove.isPending}
          onConfirm={() => remove.mutate(deleting.id, { onSuccess: () => setDeleting(null) })}
          onClose={() => setDeleting(null)}
        />
      ) : null}
    </Card>
  );
}

function SavedReplyDialog({ reply, onClose }: { reply: SavedReply | null; onClose: () => void }) {
  const save = useSaveSavedReply();
  const [name, setName] = useState(reply?.name ?? "");
  const [body, setBody] = useState(reply?.body ?? "");
  return (
    <Modal
      title={reply ? "Edit saved reply" : "New saved reply"}
      onClose={onClose}
      footer={
        <>
          <Button variant="ghost" onClick={onClose}>
            Cancel
          </Button>
          <Button variant="primary" loading={save.isPending} disabled={!name.trim() || !body.trim()} onClick={() => save.mutate({ id: reply?.id, name, body }, { onSuccess: onClose })}>
            Save
          </Button>
        </>
      }
    >
      <div className="space-y-4">
        <Field label="Name">
          <Input value={name} onChange={(e) => setName(e.target.value)} maxLength={80} placeholder="Opening hours" />
        </Field>
        <Field label="Text" hint="{{ first_name|default('there') }} fills in their first name when it's sent.">
          <Textarea value={body} onChange={(e) => setBody(e.target.value)} rows={4} maxLength={1600} placeholder="Hi {{ first_name|default('there') }}, we're open 9 to 5, Monday to Friday." />
        </Field>
        <InlineError message={save.error ? errorMessage(save.error) : null} />
      </div>
    </Modal>
  );
}
