import { useState } from "react";
import { Pencil, Plus, Trash2 } from "lucide-react";
import { errorMessage, useSession } from "../../api";
import { Button, Card, Checkbox, ConfirmDialog, EmptyState, Field, IconButton, InlineError, Input, Modal, Select, Skeleton, Textarea, Toggle } from "../../components/ui";
import { toast } from "../../lib/toast";
import { useDeleteSavedReply, useInboxSettings, useSaveInboxSettings, useSavedReplies, useSaveSavedReply } from "./api";
import type { InboxSettings, InboxSettingsPatch, SavedReply } from "./types";

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
  const set = (patch: Partial<InboxSettings>) => setS((v) => ({ ...v, ...patch }));
  const submit = () => {
    const patch: InboxSettingsPatch = {
      program_name: s.program_name,
      message_frequency: s.message_frequency,
      default_reply_enabled: s.default_reply_enabled,
      default_reply: s.default_reply,
      away_reply_enabled: s.away_reply_enabled,
      away_reply: s.away_reply,
      open_hour: s.open_hour,
      close_hour: s.close_hour,
      open_days: s.open_days,
      notify_user_ids: s.notify_user_ids,
    };
    save.mutate(patch, { onSuccess: () => toast.success("Inbox settings saved") });
  };
  const toggleDay = (d: number) => set({ open_days: s.open_days.includes(d) ? s.open_days.filter((x) => x !== d) : [...s.open_days, d].sort() });
  const toggleMember = (id: string) => set({ notify_user_ids: s.notify_user_ids.includes(id) ? s.notify_user_ids.filter((x) => x !== id) : [...s.notify_user_ids, id] });

  return (
    <>
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
      <Card title="Automatic replies">
        <div className="space-y-4">
          <ReplySetting
            title="Default reply"
            description="Answers texts that don't match a keyword. Each person gets it at most once every 12 hours, so two automated numbers can't text each other forever."
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
