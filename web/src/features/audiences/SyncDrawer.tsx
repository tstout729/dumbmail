import { useState } from "react";
import { AlertTriangle, Pause, Play, RefreshCw, Trash2 } from "lucide-react";
import { useSession } from "../../api";
import { Badge, Button, Checkbox, ConfirmDialog, Drawer, Field, Notice, Select, Skeleton, type Tone } from "../../components/ui";
import { formatDateTime, formatNumber, formatRelative } from "../../lib/format";
import { toast } from "../../lib/toast";
import { useAudienceSync, useDeleteSync, useRunSync, useUpdateSync } from "./api";
import type { AudienceRun, AudienceSync, MatchFields } from "./types";

export const intervalOptions = [
  { hours: 1, label: "Every hour" },
  { hours: 3, label: "Every 3 hours" },
  { hours: 6, label: "Every 6 hours" },
  { hours: 12, label: "Every 12 hours" },
  { hours: 24, label: "Once a day" },
  { hours: 168, label: "Once a week" },
];

export function syncState(s: AudienceSync): { label: string; tone: Tone } {
  if (s.running) return { label: "Syncing", tone: "blue" };
  if (s.status === "paused") return { label: "Paused", tone: "amber" };
  if (s.connection_status !== "connected") return { label: "Reconnect needed", tone: "amber" };
  if (s.error) return { label: "Problem", tone: "red" };
  if (!s.last_success_at) return { label: "Waiting", tone: "gray" };
  return { label: "Up to date", tone: "green" };
}

function sizeText(s: AudienceSync): string {
  if (s.platform_size === null) return "—";
  if (s.platform_size_upper !== null && s.platform_size_upper !== s.platform_size) return `${formatNumber(s.platform_size)}–${formatNumber(s.platform_size_upper)}`;
  return formatNumber(s.platform_size);
}

const triggerLabel: Record<AudienceRun["trigger"], string> = { created: "First sync", scheduled: "Scheduled", manual: "Sync now" };

export function SyncDrawer({ id, onClose }: { id: string; onClose: () => void }) {
  const { account } = useSession();
  const sync = useAudienceSync(id);
  const update = useUpdateSync(id);
  const run = useRunSync();
  const remove = useDeleteSync();
  const [deleting, setDeleting] = useState(false);
  const [alsoOnPlatform, setAlsoOnPlatform] = useState(false);
  const [widen, setWiden] = useState<{ include_unsubscribed?: boolean; match_fields?: MatchFields } | null>(null);
  const s = sync.data;

  const save = (input: Parameters<typeof update.mutate>[0]) => update.mutate(input, { onSuccess: () => toast.success("Sync updated") });

  return (
    <Drawer
      title={s?.name ?? "Audience sync"}
      onClose={onClose}
      width="w-[min(520px,100vw)]"
      footer={
        s ? (
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
            </div>
            <p className="text-zinc-600">
              {s.source.type === "list" ? "List" : "Segment"} <span className="font-medium text-zinc-900">{s.source.name ?? "(deleted)"}</span> to{" "}
              <span className="font-medium text-zinc-900">{s.platform_label}</span> ad account {s.ad_account_name ? `${s.ad_account_name} (${s.ad_account_id})` : s.ad_account_id}
            </p>
            {s.audience_id ? <p className="text-xs text-zinc-500">Audience ID on {s.platform_label}: {s.audience_id}</p> : null}
          </div>

          {s.error ? (
            <Notice tone="red" icon={<AlertTriangle />} title="Last sync didn't finish">
              {s.error}
            </Notice>
          ) : null}

          <dl className="grid grid-cols-2 gap-3">
            <Metric label="People sent" value={formatNumber(s.members_sent)} sub={s.last_run_at ? `+${formatNumber(s.last_added)} added, −${formatNumber(s.last_removed)} removed last time` : "Not synced yet"} />
            <Metric label={`Size on ${s.platform_label}`} value={sizeText(s)} sub={s.platform === "meta" ? "Meta's estimate of matched people" : "Matched people"} />
            <Metric label="Match rate" value={s.match_rate !== null ? `${Math.round(s.match_rate)}%` : "—"} sub={s.match_rate !== null ? "Reported by Google" : `${s.platform_label} doesn't report one`} />
            <Metric
              label="Last sync"
              value={s.running ? "Now" : formatRelative(s.last_run_at)}
              sub={s.status === "paused" ? "Paused" : s.next_run_at ? `Next ${formatRelative(s.next_run_at)}` : undefined}
            />
          </dl>

          <div className="space-y-3">
            <Field label="How often">
              <Select value={s.interval_hours} onChange={(e) => save({ interval_hours: Number(e.target.value) })}>
                {intervalOptions.some((o) => o.hours === s.interval_hours) ? null : <option value={s.interval_hours}>Every {s.interval_hours} hours</option>}
                {intervalOptions.map((o) => (
                  <option key={o.hours} value={o.hours}>
                    {o.label}
                  </option>
                ))}
              </Select>
            </Field>
            <Field label="Match on" hint="More fields can raise the match rate.">
              <Select
                value={s.match_fields}
                onChange={(e) => {
                  const v = e.target.value as MatchFields;
                  if (v === "contact_and_location") setWiden({ match_fields: v });
                  else save({ match_fields: v });
                }}
              >
                <option value="contact">{s.platform === "tiktok" ? "Email" : "Email and phone"}</option>
                {s.platform !== "tiktok" ? <option value="contact_and_location">Email, phone, name and location</option> : null}
              </Select>
            </Field>
            <Checkbox
              label="Exclusion audience: include people who unsubscribed"
              description="Only for audiences you exclude from ads. People who opted out of ads are still never sent."
              checked={s.include_unsubscribed}
              onChange={(e) => (e.target.checked ? setWiden({ include_unsubscribed: true }) : save({ include_unsubscribed: false }))}
            />
          </div>

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
                      {triggerLabel[r.trigger]} · +{formatNumber(r.added)} / −{formatNumber(r.removed)}
                      {r.skipped ? ` · ${formatNumber(r.skipped)} left out (no usable contact details or no ad consent)` : ""}
                      {r.invalid ? ` · ${formatNumber(r.invalid)} not readable by the platform` : ""}
                    </div>
                    {r.error ? <div className="mt-0.5 text-xs text-red-700">{r.error}</div> : null}
                  </li>
                ))}
              </ul>
            )}
          </div>
        </div>
      )}
      {widen && s ? (
        <ConfirmDialog
          title="Send more to the ad platform?"
          body={
            widen.include_unsubscribed
              ? `People who unsubscribed from your email will be sent to ${s.platform_label}. Use this only for an audience you exclude from your ads.`
              : `Hashed names, cities, states, ZIP codes and countries will also be sent to ${s.platform_label} to improve matching.`
          }
          confirmLabel="Send them"
          onConfirm={() => {
            save(widen);
            setWiden(null);
          }}
          onClose={() => setWiden(null)}
        />
      ) : null}
      {deleting && s ? (
        <ConfirmDialog
          title={`Delete "${s.name}"?`}
          body={
            <div className="space-y-3">
              <p>DumbMail stops updating this audience.</p>
              {s.audience_id ? (
                <Checkbox
                  label={`Also delete the audience on ${s.platform_label}`}
                  description="Ads that use it stop running. Leave this off to keep the audience as it is now."
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
                onSuccess: () => {
                  toast.success(alsoOnPlatform ? `Sync and ${s.platform_label} audience deleted` : "Sync deleted");
                  onClose();
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
