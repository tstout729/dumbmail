import { useState } from "react";
import { Link } from "react-router";
import { Copy, GitMerge } from "lucide-react";
import { errorMessage, useSession } from "../../api";
import { Badge, Button, Card, EmptyState, InlineError, Page, PageHeader, Skeleton, StatusBadge } from "../../components/ui";
import { formatDate, formatPhone, fullName, pluralize } from "../../lib/format";
import { useDuplicates } from "./api";
import { MergeDialog } from "./MergeDialog";
import type { DuplicateGroup } from "./types";
import { useCan } from "../team/api";

const reasonLabels: Record<DuplicateGroup["reason"], string> = {
  email: "Same email",
  phone_number: "Same phone",
  external_id: "Same external ID",
  linked_identifiers: "Linked by a sign-up",
};

function keyText(g: DuplicateGroup): string {
  if (typeof g.key === "string") return g.reason === "phone_number" ? formatPhone(`+${g.key}`) : g.key;
  if (g.key && typeof g.key === "object") return Object.values(g.key as Record<string, unknown>).map(String).join(" + ");
  return "";
}

export function DuplicatesPage() {
  const canMerge = useCan("audience.write");
  const { account } = useSession();
  const dupes = useDuplicates();
  const [merging, setMerging] = useState<{ keep: string; merge: string } | null>(null);
  const groups = dupes.data?.data ?? [];

  return (
    <Page>
      <PageHeader
        title="Possible duplicates"
        description="Profiles that look like the same person. Nothing is merged until you review it."
      />
      {dupes.error ? <InlineError message={errorMessage(dupes.error)} /> : null}
      {dupes.isPending ? (
        <div className="space-y-3">
          <Skeleton className="h-28" />
          <Skeleton className="h-28" />
        </div>
      ) : groups.length === 0 ? (
        <Card>
          <EmptyState
            icon={<Copy className="size-5" />}
            title="No duplicates found"
            description="We look for the same Gmail inbox spelled differently, the same phone number or external ID written differently, and sign-ups that link an email on one profile to a phone number on another."
          />
        </Card>
      ) : (
        <div className="space-y-3">
          <p className="text-sm text-zinc-600">{pluralize(groups.length, "group")} to review. The oldest profile is suggested as the one to keep; you can swap before merging.</p>
          {groups.map((g, i) => {
            const [first, ...rest] = g.profiles;
            return (
              <Card key={`${g.reason}-${i}`} padded={false}>
                <div className="flex flex-wrap items-center gap-2 border-b border-zinc-100 px-4 py-2.5">
                  <Badge tone={g.reason === "linked_identifiers" ? "amber" : "blue"}>{reasonLabels[g.reason]}</Badge>
                  <span className="min-w-0 truncate text-sm font-medium text-zinc-900">{keyText(g)}</span>
                  {g.times_seen && g.times_seen > 1 ? <span className="text-xs text-zinc-500">seen {g.times_seen} times</span> : null}
                </div>
                <p className="px-4 pt-2 text-xs text-zinc-500">{g.why}.</p>
                <ul className="divide-y divide-zinc-100">
                  {g.profiles.map((p) => (
                    <li key={p.id} className="flex flex-wrap items-center justify-between gap-3 px-4 py-2.5">
                      <Link to={`/audience/profiles/${p.id}`} className="min-w-0 hover:underline">
                        <span className="block truncate text-sm text-zinc-900">{fullName(p) || p.email || (p.phone_number ? formatPhone(p.phone_number) : null) || p.external_id || "Anonymous"}</span>
                        <span className="block truncate text-xs text-zinc-500">
                          {[p.email, p.phone_number ? formatPhone(p.phone_number) : null, p.external_id].filter(Boolean).join(" · ")} · added {formatDate(p.created_at, account.timezone)}
                        </span>
                      </Link>
                      <span className="flex items-center gap-2">
                        <StatusBadge status={p.email_consent} />
                        {first && p.id !== first.id && canMerge ? (
                          <Button size="sm" icon={<GitMerge className="size-3.5" />} onClick={() => setMerging({ keep: first.id, merge: p.id })}>
                            Review merge
                          </Button>
                        ) : rest.length > 0 ? (
                          <span className="w-[116px] text-right text-xs text-zinc-400">Suggested to keep</span>
                        ) : null}
                      </span>
                    </li>
                  ))}
                </ul>
              </Card>
            );
          })}
        </div>
      )}
      {merging ? <MergeDialog keepId={merging.keep} mergeId={merging.merge} onClose={() => setMerging(null)} /> : null}
    </Page>
  );
}
