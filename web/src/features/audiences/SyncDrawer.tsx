import { useState } from "react";
import { AlertTriangle, Clock, Pause, Play, Plus, RefreshCw, Trash2 } from "lucide-react";
import { useSession } from "../../api";
import { useCan } from "../team/api";
import { Badge, Button, Checkbox, ConfirmDialog, Drawer, Field, Notice, Select, Skeleton, type Tone } from "../../components/ui";
import { formatDateTime, formatNumber, formatRelative } from "../../lib/format";
import { toast } from "../../lib/toast";
import { useAudienceSync, useDeleteSync, useRunSync, useUpdateSync } from "./api";
import type { AudienceRun, AudienceSync, WaitingBatch } from "./types";

export const intervalOptions = [
  { hours: 1, label: "Every hour" },
  { hours: 3, label: "Every 3 hours" },
  { hours: 6, label: "Every 6 hours" },
  { hours: 12, label: "Every 12 hours" },
  { hours: 24, label: "Once a day" },
  { hours: 168, label: "Once a week" },
];

const pausedLabel: Record<string, string> = {
  user: "Paused",
  google_consent: "Paused: Google consent setting is off",
  tiktok_minimum: "Paused: below TikTok's minimum",
  audience_missing: "Paused: audience not found",
  create_unclear: "Paused: check TikTok",
};

export function syncState(s: AudienceSync): { label: string; tone: Tone } {
  if (s.removing) return { label: "Removing", tone: "amber" };
  if (s.running) return { label: "Syncing", tone: "blue" };
  if (s.status === "paused") return { label: pausedLabel[s.pause_reason ?? "user"] ?? "Paused", tone: "amber" };
  if (s.connection_status !== "connected") return { label: "Reconnect needed", tone: "amber" };
  if (s.error) return { label: "Problem", tone: "red" };
  if (!s.last_success_at) return { label: "Waiting", tone: "gray" };
  if (s.pending_removal > 0) return { label: "Removals pending", tone: "amber" };
  return { label: "Up to date", tone: "green" };
}

function sizeText(s: AudienceSync): string {
  if (s.platform_size === null) return "—";
  if (s.platform_size_upper !== null && s.platform_size_upper !== s.platform_size) return `${formatNumber(s.platform_size)}–${formatNumber(s.platform_size_upper)}`;
  return formatNumber(s.platform_size);
}

const triggerLabel: Record<AudienceRun["trigger"], string> = { created: "First sync", scheduled: "Scheduled", manual: "Sync now", deleted: "Deleting" };

const modeLabel: Record<AudienceRun["mode"], string> = {
  full: "",
  remove_only: " · removals only (paused)",
  veto_only: " · opt-outs only (list or segment gone)",
  withdraw: " · removing everyone (sync being deleted)",
};

function waitingText(w: WaitingBatch, platform: string): string {
  const n = formatNumber(w.count);
  if (w.status === "accepted") return `${n} ${w.op === "add" ? "additions" : "removals"} taken by ${platform}, waiting for it to confirm`;
  return `${n} ${w.op === "add" ? "additions" : "removals"} waiting to be resent (${platform}'s last answer couldn't be read)`;
}

export function SyncDrawer({ id, onClose }: { id: string; onClose: () => void }) {
  const { account } = useSession();
  const sync = useAudienceSync(id);
  const update = useUpdateSync(id);
  const run = useRunSync();
  const remove = useDeleteSync();
  const [deleting, setDeleting] = useState(false);
  const [alsoOnPlatform, setAlsoOnPlatform] = useState(false);
  const [recreating, setRecreating] = useState(false);
  const canManage = useCan("ad_audiences.manage");
  const s = sync.data;

  const save = (input: Parameters<typeof update.mutate>[0]) => update.mutate(input, { onSuccess: () => toast.success("Sync updated") });

  return (
    <Drawer
      title={s?.name ?? "Audience sync"}
      onClose={onClose}
      width="w-[min(520px,100vw)]"
      footer={
        s && canManage && s.removing ? (
          <>
            <Button
              icon={<RefreshCw className="size-3.5" />}
              loading={run.isPending}
              disabled={s.running || s.connection_status !== "connected"}
              onClick={() => run.mutate(s.id, { onSuccess: () => toast.success("Trying the removal again") })}
            >
              {s.running ? "Removing…" : "Try again now"}
            </Button>
            {s.delete_audience && s.platform !== "tiktok" ? (
              <Button
                variant="ghost"
                className="ml-auto"
                loading={remove.isPending}
                disabled={s.running}
                onClick={() => remove.mutate({ id: s.id, deleteOnPlatform: false }, { onSuccess: () => toast.success(`The ${s.platform_label} audience will be kept, empty`) })}
              >
                Keep the audience instead
              </Button>
            ) : null}
          </>
        ) : s && canManage ? (
          <>
            <Button
              variant="primary"
              icon={<RefreshCw className="size-3.5" />}
              loading={run.isPending}
              disabled={s.running || s.status === "paused" || s.connection_status !== "connected"}
              onClick={() => run.mutate(s.id, { onSuccess: () => toast.success("Sync started") })}
            >
              {s.running ? "Syncing…" : "Sync now"}
            </Button>
            {s.status === "active" ? (
              <Button icon={<Pause className="size-3.5" />} onClick={() => save({ status: "paused" })}>
                Pause
              </Button>
            ) : s.pause_reason === "audience_missing" || s.pause_reason === "create_unclear" ? (
              <Button icon={<Plus className="size-3.5" />} onClick={() => setRecreating(true)}>
                Create a new audience
              </Button>
            ) : (
              <Button icon={<Play className="size-3.5" />} onClick={() => save({ status: "active" })}>
                Resume
              </Button>
            )}
            <Button variant="ghost" className="ml-auto text-red-700" icon={<Trash2 className="size-3.5" />} onClick={() => setDeleting(true)}>
              Delete
            </Button>
          </>
        ) : null
      }
    >
      {sync.isPending ? (
        <div className="space-y-3">
          <Skeleton className="h-5 w-56" />
          <Skeleton className="h-20 w-full" />
        </div>
      ) : sync.error || !s ? (
        <p className="text-sm text-red-600">{sync.error?.message ?? "This sync doesn't exist."}</p>
      ) : (
        <div className="space-y-5">
          <div className="space-y-1 text-sm">
            <div className="flex flex-wrap items-center gap-2">
              <Badge tone={syncState(s).tone} dot>
                {syncState(s).label}
              </Badge>
              {s.platform_status ? <span className="text-xs text-zinc-500">{s.platform_label}: {s.platform_status}</span> : null}
              {!s.verified_live ? <Badge tone="amber">Not yet verified against a live account</Badge> : null}
            </div>
            <p className="text-zinc-600">
              {s.source.type === "list" ? "List" : "Segment"} <span className="font-medium text-zinc-900">{s.source.name ?? "(deleted)"}</span> to{" "}
              <span className="font-medium text-zinc-900">{s.platform_label}</span> ad account {s.ad_account_name ? `${s.ad_account_name} (${s.ad_account_id})` : s.ad_account_id}
            </p>
            {s.audience_id ? <p className="text-xs text-zinc-500">Audience ID on {s.platform_label}: {s.audience_id}</p> : null}
          </div>

          {s.removing_message ? (
            <Notice tone="amber" icon={<Trash2 />} title="Removing">
              {s.removing_message}
              {s.connection_status !== "connected" ? ` Reconnect ${s.platform_label} so DumbMail can finish.` : ""}
            </Notice>
          ) : null}
          {s.pause_message && s.pause_reason !== "user" ? (
            <Notice tone="amber" icon={<Pause />} title="Paused">
              {s.pause_message}
            </Notice>
          ) : null}
          {s.error ? (
            <Notice tone="red" icon={<AlertTriangle />} title="Last sync had a problem">
              {s.error}
            </Notice>
          ) : null}
          {s.waiting.length > 0 ? (
            <Notice tone="blue" icon={<Clock />} title="Waiting for the platform">
              {s.waiting.map((w) => waitingText(w, s.platform_label)).join(". ")}.
            </Notice>
          ) : null}

          <dl className="grid grid-cols-2 gap-3">
            <Metric
              label={`Confirmed by ${s.platform_label}`}
              value={formatNumber(s.confirmed)}
              sub={s.last_run_at ? `+${formatNumber(s.last_added)} added, −${formatNumber(s.last_removed)} removed last time (confirmed)` : "Not synced yet"}
            />
            <Metric label="Sent, not confirmed" value={formatNumber(Math.max(0, s.sent - s.confirmed))} sub="Treated as in the audience until confirmed" />
            <Metric label="Still pending removal" value={formatNumber(s.pending_removal)} sub={s.pending_removal > 0 ? `Not removed until ${s.platform_label} confirms` : "Nothing waiting"} />
            <Metric label={`Size on ${s.platform_label}`} value={sizeText(s)} sub={`${s.platform_label}'s estimate of matched people`} />
            <Metric label="Match rate" value={s.match_rate !== null ? `${Math.round(s.match_rate)}%` : "—"} sub={s.match_rate !== null ? "Reported by Google" : `${s.platform_label} doesn't report one`} />
            <Metric
              label="Last sync"
              value={s.running ? "Now" : formatRelative(s.last_run_at)}
              sub={s.status === "paused" ? "Paused" : s.next_run_at ? `Next ${formatRelative(s.next_run_at)}` : undefined}
            />
          </dl>

          {canManage && !s.removing ? (
            <Field label="How often" hint={`Matched on ${s.platform === "tiktok" ? "email" : "email and phone"} only.`}>
              <Select value={s.interval_hours} onChange={(e) => save({ interval_hours: Number(e.target.value) })}>
                {intervalOptions.some((o) => o.hours === s.interval_hours) ? null : <option value={s.interval_hours}>Every {s.interval_hours} hours</option>}
                {intervalOptions.map((o) => (
                  <option key={o.hours} value={o.hours}>
                    {o.label}
                  </option>
                ))}
              </Select>
            </Field>
          ) : null}

          <div>
            <h3 className="mb-2 text-xs font-semibold uppercase tracking-wide text-zinc-500">History</h3>
            {s.runs.length === 0 ? (
              <p className="text-sm text-zinc-500">No syncs yet.</p>
            ) : (
              <ul className="divide-y divide-zinc-100 rounded-lg border border-zinc-200">
                {s.runs.map((r) => (
                  <li key={r.id} className="px-3 py-2 text-sm">
                    <div className="flex flex-wrap items-center justify-between gap-2">
                      <span className="text-zinc-800">{formatDateTime(r.started_at, account.timezone)}</span>
                      <RunBadge run={r} />
                    </div>
                    <div className="text-xs text-zinc-500">
                      {triggerLabel[r.trigger]}
                      {modeLabel[r.mode]} · +{formatNumber(r.added)} / −{formatNumber(r.removed)} confirmed
                      {r.skipped ? ` · ${formatNumber(r.skipped)} had nothing the platform can match on` : ""}
                      {r.rejected ? ` · ${formatNumber(r.rejected)} not confirmed by the platform` : ""}
                    </div>
                    {r.error ? <div className="mt-0.5 text-xs text-red-700">{r.error}</div> : null}
                  </li>
                ))}
              </ul>
            )}
          </div>
        </div>
      )}
      {recreating && s ? (
        <ConfirmDialog
          title="Create a new audience?"
          body={`DumbMail makes a new audience on ${s.platform_label} and uploads everyone who qualifies. ${
            s.pause_reason === "create_unclear" ? `Delete any audience called "${s.name}" that's already on ${s.platform_label} first.` : ""
          }`}
          confirmLabel="Create and upload"
          onConfirm={() => {
            save({ recreate_audience: true, confirm: true });
            setRecreating(false);
          }}
          onClose={() => setRecreating(false)}
        />
      ) : null}
      {deleting && s ? (
        <ConfirmDialog
          title={`Delete "${s.name}"?`}
          body={
            <div className="space-y-3">
              <p>
                DumbMail first removes everyone it uploaded from the audience on {s.platform_label}, so people who later unsubscribe or opt out are never left in it. The
                sync shows as Removing until {s.platform_label} confirms every removal, and DumbMail keeps trying until it does.
              </p>
              {s.audience_id && s.platform === "tiktok" ? (
                <p>TikTok can't keep an audience with fewer than 1,000 people, so the audience on TikTok is deleted too.</p>
              ) : s.audience_id ? (
                <Checkbox
                  label={`Then delete the audience on ${s.platform_label}`}
                  description="Ads that use it stop running. Leave this off to keep the empty audience."
                  checked={alsoOnPlatform}
                  onChange={(e) => setAlsoOnPlatform(e.target.checked)}
                />
              ) : null}
            </div>
          }
          confirmLabel="Delete sync"
          danger
          loading={remove.isPending}
          onConfirm={() =>
            remove.mutate(
              { id: s.id, deleteOnPlatform: alsoOnPlatform },
              {
                onSuccess: (r) => {
                  toast.success(r.deleted ? "Sync deleted" : `Removing everyone from the ${s.platform_label} audience`);
                  setDeleting(false);
                  if (r.deleted) onClose();
                },
              },
            )
          }
          onClose={() => setDeleting(false)}
        />
      ) : null}
    </Drawer>
  );
}

function Metric({ label, value, sub }: { label: string; value: string; sub?: string }) {
  return (
    <div className="rounded-lg border border-zinc-200 px-3 py-2">
      <dt className="text-xs font-medium text-zinc-500">{label}</dt>
      <dd className="mt-0.5 text-lg font-semibold tabular-nums text-zinc-950">{value}</dd>
      {sub ? <dd className="text-xs text-zinc-500">{sub}</dd> : null}
    </div>
  );
}

function RunBadge({ run }: { run: AudienceRun }) {
  const map: Record<AudienceRun["status"], { label: string; tone: Tone }> = {
    running: { label: "Running", tone: "blue" },
    done: { label: "Done", tone: "green" },
    failed: { label: "Didn't finish", tone: "red" },
    stopped: { label: "Stopped", tone: "gray" },
  };
  const m = map[run.status];
  return <Badge tone={m.tone}>{m.label}</Badge>;
}
