import { useState } from "react";
import { ArrowLeftRight, Search } from "lucide-react";
import { useProfiles } from "../../api";
import { Badge, Button, InlineError, Input, Modal, Skeleton, StatusBadge } from "../../components/ui";
import { errorMessage } from "../../api";
import { formatNumber, formatPhone, fullName } from "../../lib/format";
import { useDebouncedValue } from "../../lib/hooks";
import { toast } from "../../lib/toast";
import { useMerge, useMergePreview } from "./api";
import type { MergeField, MergeRow } from "./types";

const fieldLabels: Record<string, string> = {
  email: "Email",
  phone_number: "Phone",
  external_id: "External ID",
  first_name: "First name",
  last_name: "Last name",
};

const tableLabels: Record<string, string> = {
  events: "events",
  messages: "messages",
  list_members: "list memberships",
  segment_members: "segment memberships",
  flow_runs: "flow runs",
  optin_confirmations: "sign-up confirmations",
  consent_events: "consent history entries",
  profile_notes: "notes",
};

function show(v: unknown): string {
  if (v === null || v === undefined || v === "") return "—";
  if (typeof v === "string") return v;
  return JSON.stringify(v);
}

function label(r: MergeRow): string {
  return fullName(r) || r.email || (r.phone_number ? formatPhone(r.phone_number) : null) || r.external_id || "Anonymous";
}

/** Review and merge two profiles. The kept profile's values win unless a field is switched. */
export function MergeDialog({ keepId, mergeId, onClose, onMerged }: { keepId: string; mergeId: string; onClose: () => void; onMerged?: (keptId: string) => void }) {
  const [ids, setIds] = useState({ keep: keepId, merge: mergeId });
  const [choices, setChoices] = useState<Record<string, "keep" | "merge">>({});
  const preview = useMergePreview({ keep_id: ids.keep, merge_id: ids.merge, choices });
  const merge = useMerge();
  const p = preview.data;
  const conflicts = (p?.fields ?? []).filter((f) => f.conflict);
  const moving = Object.entries(p?.rows_moving ?? {}).filter(([t]) => tableLabels[t]);

  const choose = (f: MergeField, side: "keep" | "merge") => setChoices({ ...choices, [f.field]: side });

  return (
    <Modal
      title="Merge profiles"
      description="Everything moves to the profile you keep, then the other one is removed."
      onClose={onClose}
      size="lg"
      footer={
        <>
          <Button variant="ghost" onClick={onClose}>
            Cancel
          </Button>
          <Button
            variant="primary"
            disabled={!p}
            loading={merge.isPending}
            onClick={() =>
              merge.mutate(
                { keep_id: ids.keep, merge_id: ids.merge, choices, confirm: true },
                {
                  onSuccess: () => {
                    toast.success("Profiles merged");
                    onMerged?.(ids.keep);
                    onClose();
                  },
                },
              )
            }
          >
            Merge profiles
          </Button>
        </>
      }
    >
      {preview.error ? <InlineError message={errorMessage(preview.error)} /> : null}
      {!p ? (
        <Skeleton className="h-48" />
      ) : (
        <div className="space-y-4 text-sm">
          <div className="grid grid-cols-1 items-center gap-2 sm:grid-cols-[1fr_auto_1fr]">
            <div className="rounded-lg border border-accent-200 bg-accent-50/50 p-3">
              <div className="text-[11px] font-semibold uppercase tracking-wide text-accent-800">Keep</div>
              <div className="truncate font-medium text-zinc-900">{label(p.keep)}</div>
              <div className="truncate text-xs text-zinc-500">{p.keep.email ?? p.keep.phone_number ?? "—"}</div>
            </div>
            <Button
              size="sm"
              variant="ghost"
              icon={<ArrowLeftRight className="size-3.5" />}
              onClick={() => {
                setIds({ keep: ids.merge, merge: ids.keep });
                setChoices({});
              }}
            >
              Swap
            </Button>
            <div className="rounded-lg border border-zinc-200 p-3">
              <div className="text-[11px] font-semibold uppercase tracking-wide text-zinc-500">Merge in and remove</div>
              <div className="truncate font-medium text-zinc-900">{label(p.merge)}</div>
              <div className="truncate text-xs text-zinc-500">{p.merge.email ?? p.merge.phone_number ?? "—"}</div>
            </div>
          </div>

          {conflicts.length > 0 ? (
            <div>
              <div className="mb-1.5 text-xs font-medium text-zinc-700">Both profiles have a value for these. Pick which one to keep.</div>
              <div className="divide-y divide-zinc-100 rounded-lg border border-zinc-200">
                {conflicts.map((f) => {
                  const side = choices[f.field] ?? "keep";
                  const name = fieldLabels[f.field] ?? f.field.replace(/^properties\./, "");
                  return (
                    <div key={f.field} className="grid grid-cols-1 gap-2 px-3 py-2 sm:grid-cols-[140px_1fr_1fr] sm:items-center">
                      <div className={f.field.startsWith("properties.") ? "font-mono text-xs text-zinc-600" : "text-zinc-600"}>{name}</div>
                      {(["keep", "merge"] as const).map((s) => (
                        <label key={s} className="flex min-w-0 cursor-pointer items-center gap-2">
                          <input type="radio" name={f.field} checked={side === s} onChange={() => choose(f, s)} className="accent-accent-700" />
                          <span className="truncate text-zinc-900" title={show(f[s])}>
                            {show(f[s])}
                          </span>
                        </label>
                      ))}
                    </div>
                  );
                })}
              </div>
            </div>
          ) : (
            <p className="text-zinc-600">No conflicting values. Anything only the other profile has is copied over.</p>
          )}

          <div className="rounded-lg bg-zinc-50 p-3">
            <div className="mb-1.5 text-xs font-medium text-zinc-700">After the merge</div>
            <div className="flex flex-wrap items-center gap-x-4 gap-y-1.5">
              <span className="flex items-center gap-1.5">
                Email <StatusBadge status={p.result.email_consent} />
              </span>
              <span className="flex items-center gap-1.5">
                SMS <StatusBadge status={p.result.sms_consent} />
              </span>
              {p.result.email_suppressed_reason ? <Badge tone="red">Email suppressed</Badge> : null}
              {p.result.sms_suppressed_reason ? <Badge tone="red">Texts suppressed</Badge> : null}
              {p.result.ads_opt_out ? <Badge>Do not sell or share</Badge> : null}
            </div>
            <p className="mt-2 text-xs text-zinc-500">Consent on each channel becomes the more restrictive of the two, and suppressions carry over, so merging never makes anyone easier to reach.</p>
            {moving.length > 0 ? (
              <p className="mt-2 text-xs text-zinc-600">
                Moving: {moving.map(([t, n]) => `${formatNumber(n)} ${tableLabels[t]}`).join(", ")}.
              </p>
            ) : null}
          </div>
        </div>
      )}
    </Modal>
  );
}

/** Pick another profile to merge with this one. */
export function MergePicker({ profileId, onClose, onMerged }: { profileId: string; onClose: () => void; onMerged?: (keptId: string) => void }) {
  const [q, setQ] = useState("");
  const query = useDebouncedValue(q.trim(), 250);
  const results = useProfiles({ q: query || undefined, limit: 8 });
  const [other, setOther] = useState<string | null>(null);
  if (other) return <MergeDialog keepId={profileId} mergeId={other} onClose={onClose} onMerged={onMerged} />;
  const rows = (results.data?.data ?? []).filter((p) => p.id !== profileId);
  return (
    <Modal title="Merge with another profile" description="Find the duplicate. You'll review the result before anything changes." onClose={onClose} size="md">
      <div className="relative mb-3">
        <Search className="pointer-events-none absolute left-2.5 top-2.5 size-3.5 text-zinc-400" />
        <Input value={q} onChange={(e) => setQ(e.target.value)} placeholder="Search by email, phone, name or ID" className="pl-8" autoFocus />
      </div>
      {results.isPending ? (
        <Skeleton className="h-32" />
      ) : rows.length === 0 ? (
        <p className="py-6 text-center text-sm text-zinc-500">{query ? `Nobody else matches “${query}”.` : "Start typing to find a profile."}</p>
      ) : (
        <ul className="divide-y divide-zinc-100 rounded-lg border border-zinc-200">
          {rows.map((p) => (
            <li key={p.id}>
              <button type="button" onClick={() => setOther(p.id)} className="flex w-full items-center justify-between gap-3 px-3 py-2 text-left hover:bg-zinc-50">
                <span className="min-w-0">
                  <span className="block truncate text-sm text-zinc-900">{fullName(p) || p.email || p.phone_number || "Anonymous"}</span>
                  <span className="block truncate text-xs text-zinc-500">{[p.email, p.phone_number ? formatPhone(p.phone_number) : null].filter(Boolean).join(" · ") || "—"}</span>
                </span>
                <span className="text-xs font-medium text-accent-700">Review</span>
              </button>
            </li>
          ))}
        </ul>
      )}
    </Modal>
  );
}
