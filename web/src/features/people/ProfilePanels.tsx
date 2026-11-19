import { useState, type FormEvent, type ReactNode } from "react";
import { useNavigate } from "react-router";
import { CheckCircle2, Download, GitMerge, History, Loader2, Mail, MessageSquare, Trash2, XCircle } from "lucide-react";
import { Button, Card, ConfirmDialog, IconButton, Modal, Skeleton, Textarea, Toggle, cx } from "../../components/ui";
import { formatDate, formatDateTime, formatPhone, formatRelative } from "../../lib/format";
import { toast } from "../../lib/toast";
import { useCan } from "../team/api";
import { useAddNote, useConsentPage, useDeleteNote, useExportPerson, usePeopleSummary, usePrivacyRequest, useSetAdsOptOut } from "./api";
import { MergePicker } from "./MergeDialog";
import type { ChannelReach, ConsentEvent, ExportResult } from "./types";

type PanelProps = { profileId: string; email: string | null; phone: string | null; tz: string };

function ReachRow({ icon, label, reach, tz }: { icon: ReactNode; label: string; reach: ChannelReach; tz: string }) {
  return (
    <div>
      <div className="flex items-center gap-2 text-sm font-medium text-zinc-900">
        {icon}
        {label}
        {reach.can_send ? (
          <span className="ml-auto flex items-center gap-1 text-xs font-medium text-accent-700">
            <CheckCircle2 className="size-3.5" /> Yes
          </span>
        ) : (
          <span className="ml-auto flex items-center gap-1 text-xs font-medium text-red-700">
            <XCircle className="size-3.5" /> No
          </span>
        )}
      </div>
      {reach.reasons.length > 0 ? (
        <ul className="mt-1 space-y-1 pl-6 text-xs text-zinc-600">
          {reach.reasons.map((r) => (
            <li key={r.code}>
              {r.text}
              {r.since ? <span className="text-zinc-500"> Since {formatDateTime(r.since, tz)}{r.how ? `, ${r.how.charAt(0).toLowerCase()}${r.how.slice(1)}` : ""}.</span> : null}
            </li>
          ))}
        </ul>
      ) : null}
    </div>
  );
}

/** Left-column cards on the profile page: reachability, privacy and notes. */
export function ProfileSidePanels({ profileId, email, phone, tz }: PanelProps) {
  const summary = usePeopleSummary(profileId);
  const ads = useSetAdsOptOut(profileId);
  const canPrivacy = useCan("privacy.manage");
  const canWrite = useCan("audience.write");
  const exportPerson = useExportPerson();
  const [exported, setExported] = useState<ExportResult | null>(null);
  const [merging, setMerging] = useState(false);
  const navigate = useNavigate();

  if (summary.isPending) return <Skeleton className="h-40" />;
  if (summary.error) return <p className="text-sm text-red-600">{summary.error.message}</p>;
  const s = summary.data;

  return (
    <>
      <Card title="Can they be reached?">
        <div className="space-y-3">
          <ReachRow icon={<Mail className="size-3.5 text-zinc-400" />} label="Email" reach={s.reachability.email} tz={tz} />
          <ReachRow icon={<MessageSquare className="size-3.5 text-zinc-400" />} label="Text messages" reach={s.reachability.sms} tz={tz} />
        </div>
      </Card>
      <Card title="Privacy">
        <div className="space-y-3 text-sm">
          <div className="flex items-start justify-between gap-3">
            <div>
              <div className="font-medium text-zinc-900">Do not sell or share</div>
              <p className="text-xs text-zinc-500">
                {s.ads_opt_out
                  ? `On${s.ads_opt_out_at ? ` since ${formatDate(s.ads_opt_out_at, tz)}` : ""}${s.ads_opt_out_source === "gpc" ? ", from their browser's privacy signal" : ""}. Ad audience syncs leave them out.`
                  : "They haven't asked. Turn on if they ask you not to sell or share their data."}
              </p>
            </div>
            <Toggle checked={s.ads_opt_out} disabled={ads.isPending || !canPrivacy} onChange={(v) => ads.mutate(v)} label="Do not sell or share" />
          </div>
          {canPrivacy || canWrite ? (
            <div className="flex flex-wrap gap-2 border-t border-zinc-100 pt-3">
              {canPrivacy ? (
                <Button
                  size="sm"
                  icon={<Download className="size-3.5" />}
                  disabled={!email && !phone}
                  loading={exportPerson.isPending}
                  onClick={() => exportPerson.mutate(email ? { email } : { phone_number: phone ?? "" }, { onSuccess: setExported })}
                >
                  Export their data
                </Button>
              ) : null}
              {canWrite ? (
                <Button size="sm" icon={<GitMerge className="size-3.5" />} onClick={() => setMerging(true)}>
                  Merge with…
                </Button>
              ) : null}
            </div>
          ) : null}
        </div>
      </Card>
      <NotesCard profileId={profileId} notes={s.notes} tz={tz} />
      {exported ? <ExportReady result={exported} tz={tz} onClose={() => setExported(null)} /> : null}
      {merging ? <MergePicker profileId={profileId} onClose={() => setMerging(false)} onMerged={(kept) => navigate(`/audience/profiles/${kept}`)} /> : null}
    </>
  );
}

/** After asking for an export: waits while it's built, then offers the downloads. */
export function ExportReady({ result, tz, onClose }: { result: ExportResult; tz: string; onClose: () => void }) {
  const req = usePrivacyRequest(result.found ? result.request_id : null);
  const r = req.data;
  const title = !result.found ? "Nothing to export" : r?.status === "ready" ? "Their data is ready" : r?.status === "failed" ? "The export didn't finish" : "Building the export…";
  return (
    <Modal title={title} onClose={onClose} size="sm" footer={<Button onClick={onClose}>{r?.status === "building" || !r ? "Close" : "Done"}</Button>}>
      {!result.found ? (
        <p className="text-sm text-zinc-600">{result.note ?? "Nobody with that address is in this account."}</p>
      ) : !r || r.status === "building" ? (
        <div className="space-y-2 text-sm text-zinc-600">
          <p className="flex items-center gap-2">
            <Loader2 className="size-3.5 animate-spin" /> Collecting everything you hold about them. This usually takes under a minute.
          </p>
          <p className="text-xs text-zinc-500">You can close this. The files appear on the Privacy requests page when they're ready.</p>
        </div>
      ) : r.status === "failed" ? (
        <p className="text-sm text-zinc-600">{typeof r.result.error === "string" ? r.result.error : "Something went wrong. Run the export again."}</p>
      ) : (
        <div className="space-y-3 text-sm text-zinc-600">
          <p>
            Download both files and send them to the person. The links need you to be signed in{r.files[0]?.expires_at ? ` and work until ${formatDate(r.files[0].expires_at, tz)}` : ""}.
          </p>
          <ul className="space-y-1.5">
            {r.files.map((f) => (
              <li key={f.id}>
                <a href={f.url} download className="inline-flex items-center gap-1.5 font-medium text-accent-700 hover:underline">
                  <Download className="size-3.5" /> {f.name}
                </a>
              </li>
            ))}
          </ul>
          <p className="text-xs text-zinc-500">The request is recorded on the Privacy requests page.</p>
        </div>
      )}
    </Modal>
  );
}

function NotesCard({ profileId, notes, tz }: { profileId: string; notes: { id: string; author: string; body: string; created_at: string }[]; tz: string }) {
  const add = useAddNote(profileId);
  const remove = useDeleteNote(profileId);
  const canWrite = useCan("audience.write");
  const [body, setBody] = useState("");
  const [confirm, setConfirm] = useState<string | null>(null);
  const onSubmit = (e: FormEvent) => {
    e.preventDefault();
    if (!body.trim()) return;
    add.mutate(body.trim(), { onSuccess: () => setBody("") });
  };
  return (
    <Card title="Notes">
      {canWrite ? (
        <form onSubmit={onSubmit} className="space-y-2">
          <Textarea value={body} onChange={(e) => setBody(e.target.value)} placeholder="Add a note for your team. The person never sees it." rows={2} maxLength={5000} aria-label="New note" />
          <div className="flex justify-end">
            <Button type="submit" size="sm" variant="primary" disabled={!body.trim()} loading={add.isPending}>
              Add note
            </Button>
          </div>
        </form>
      ) : notes.length === 0 ? (
        <p className="text-sm text-zinc-500">No notes yet.</p>
      ) : null}
      {notes.length > 0 ? (
        <ul className="mt-3 space-y-3 border-t border-zinc-100 pt-3">
          {notes.map((n) => (
            <li key={n.id} className="group text-sm">
              <div className="flex items-start justify-between gap-2">
                <p className="min-w-0 whitespace-pre-wrap break-words text-zinc-800">{n.body}</p>
                {canWrite ? (
                  <IconButton label="Delete note" size="sm" className="opacity-60 group-hover:opacity-100" onClick={() => setConfirm(n.id)}>
                    <Trash2 className="size-3.5" />
                  </IconButton>
                ) : null}
              </div>
              <div className="mt-0.5 text-xs text-zinc-500" title={formatDateTime(n.created_at, tz)}>
                {n.author} · {formatRelative(n.created_at)}
              </div>
            </li>
          ))}
        </ul>
      ) : null}
      {confirm ? (
        <ConfirmDialog
          title="Delete this note?"
          confirmLabel="Delete note"
          danger
          loading={remove.isPending}
          onConfirm={() => remove.mutate(confirm, { onSuccess: () => { setConfirm(null); toast.success("Note deleted"); } })}
          onClose={() => setConfirm(null)}
        />
      ) : null}
    </Card>
  );
}

const channelLabel: Record<ConsentEvent["channel"], string> = { email: "Email", sms: "SMS", ads: "Privacy" };

function tone(e: ConsentEvent): string {
  const v = e.new_value;
  if (e.field === "suppression" && v) return "bg-red-500 ring-red-200";
  if (v === "unsubscribed" || v === "on") return "bg-red-500 ring-red-200";
  if (v === "subscribed") return "bg-accent-500 ring-accent-200";
  return "bg-zinc-400 ring-zinc-200";
}

/** Right-column card on the profile page: every consent change, newest first. */
export function ConsentTimeline({ profileId, tz }: { profileId: string; tz: string }) {
  const summary = usePeopleSummary(profileId);
  const [offset, setOffset] = useState(0);
  const page = useConsentPage(profileId, offset, offset > 0);
  if (summary.isPending) return <Skeleton className="h-40" />;
  if (summary.error) return null;
  const s = summary.data;
  const events = offset > 0 ? (page.data?.data ?? []) : s.consent_history;
  const total = s.consent_history_total;
  return (
    <Card title="Consent history" padded={false}>
      {events.length === 0 ? (
        <div className="flex flex-col items-center px-6 py-8 text-center">
          <div className="mb-2 rounded-full bg-zinc-100 p-2.5 text-zinc-500">
            <History className="size-4" />
          </div>
          <p className="text-sm text-zinc-600">No consent changes recorded yet.</p>
        </div>
      ) : (
        <ol className="px-4 py-3">
          {events.map((e, i) => (
            <li key={e.id} className="relative flex gap-3 pb-4 last:pb-0">
              {i < events.length - 1 ? <span className="absolute left-[5px] top-4 h-full w-px bg-zinc-200" /> : null}
              <span className={cx("relative mt-1.5 size-2.5 shrink-0 rounded-full border-2 border-white ring-1", tone(e))} />
              <div className="min-w-0 flex-1">
                <div className="flex flex-wrap items-baseline justify-between gap-2">
                  <span className="text-sm font-medium text-zinc-900">{e.summary}</span>
                  <span className="text-xs text-zinc-500" title={formatDateTime(e.occurred_at, tz)}>
                    {formatDateTime(e.occurred_at, tz)}
                  </span>
                </div>
                <div className="text-xs text-zinc-600">
                  {e.how}
                  {e.address ? <span className="text-zinc-400"> · {channelLabel[e.channel]} {e.channel === "sms" ? formatPhone(e.address) : e.address}</span> : null}
                </div>
                {e.ip || e.user_agent ? (
                  <div className="mt-0.5 truncate text-[11px] text-zinc-400" title={e.user_agent ?? undefined}>
                    {[e.ip ? `IP ${e.ip}` : null, e.user_agent].filter(Boolean).join(" · ")}
                  </div>
                ) : null}
              </div>
            </li>
          ))}
        </ol>
      )}
      <div className="flex flex-wrap items-center justify-between gap-2 border-t border-zinc-100 px-4 py-2 text-xs text-zinc-500">
        <span>No history before {formatDate(s.history_started_at, tz)}. Changes can't be edited or removed.</span>
        {total > 50 ? (
          <span className="flex gap-2">
            <Button size="sm" variant="ghost" disabled={offset === 0} onClick={() => setOffset(Math.max(0, offset - 50))}>
              Newer
            </Button>
            <Button size="sm" variant="ghost" disabled={offset + 50 >= total} onClick={() => setOffset(offset + 50)}>
              Older
            </Button>
          </span>
        ) : null}
      </div>
    </Card>
  );
}
