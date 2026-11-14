import type { ReactNode } from "react";
import { ChevronLeft, ChevronRight, Users } from "lucide-react";
import type { Profile } from "../../api";
import { useSession } from "../../api";
import { formatNumber, formatRelative, fullName } from "../../lib/format";
import { EmptyRow, EmptyState, IconButton, RowLink, SkeletonRows, StatusBadge, Table, Td, Th } from "../ui";

type Props = {
  profiles: Profile[] | undefined;
  total: number;
  loading: boolean;
  offset: number;
  limit: number;
  onPage: (offset: number) => void;
  empty?: { title: string; description?: string; action?: ReactNode };
  selected?: Set<string>;
  onToggle?: (id: string) => void;
};

export function ProfilesTable({ profiles, total, loading, offset, limit, onPage, empty, selected, onToggle }: Props) {
  const { account } = useSession();
  const cols = onToggle ? 7 : 6;
  return (
    <>
      <Table>
        <thead>
          <tr>
            {onToggle ? <Th className="w-8" /> : null}
            <Th>Profile</Th>
            <Th>Phone</Th>
            <Th>Email</Th>
            <Th>SMS</Th>
            <Th>Last active</Th>
            <Th>Created</Th>
          </tr>
        </thead>
        <tbody>
          {loading && !profiles ? (
            <SkeletonRows cols={cols} rows={8} />
          ) : (profiles ?? []).length === 0 ? (
            <EmptyRow cols={cols}>
              <EmptyState icon={<Users className="size-5" />} title={empty?.title ?? "No profiles"} description={empty?.description} action={empty?.action} />
            </EmptyRow>
          ) : (
            (profiles ?? []).map((p) => (
              <RowLink key={p.id} to={`/audience/profiles/${p.id}`}>
                {onToggle ? (
                  <Td onClick={(e) => e.stopPropagation()}>
                    <input type="checkbox" checked={selected?.has(p.id) ?? false} onChange={() => onToggle(p.id)} className="size-4 accent-accent-700" aria-label="Select profile" />
                  </Td>
                ) : null}
                <Td>
                  <div className="font-medium text-zinc-900">{fullName(p) || p.email || p.phone_number || p.external_id || "Anonymous"}</div>
                  {fullName(p) && p.email ? <div className="text-xs text-zinc-500">{p.email}</div> : null}
                </Td>
                <Td className="text-zinc-600 tabular-nums">{p.phone_number ?? "—"}</Td>
                <Td>
                  <StatusBadge status={p.email_consent} />
                </Td>
                <Td>
                  <StatusBadge status={p.sms_consent} />
                </Td>
                <Td className="text-zinc-600">{p.last_event_at ? formatRelative(p.last_event_at) : "—"}</Td>
                <Td className="text-zinc-600">{new Intl.DateTimeFormat("en-US", { timeZone: account.timezone, dateStyle: "medium" }).format(new Date(p.created_at))}</Td>
              </RowLink>
            ))
          )}
        </tbody>
      </Table>
      {total > limit ? (
        <div className="mt-3 flex items-center justify-end gap-2 text-xs text-zinc-500">
          <span className="tabular-nums">
            {formatNumber(offset + 1)}–{formatNumber(Math.min(offset + limit, total))} of {formatNumber(total)}
          </span>
          <IconButton label="Previous page" disabled={offset === 0} onClick={() => onPage(Math.max(0, offset - limit))}>
            <ChevronLeft className="size-4" />
          </IconButton>
          <IconButton label="Next page" disabled={offset + limit >= total} onClick={() => onPage(offset + limit)}>
            <ChevronRight className="size-4" />
          </IconButton>
        </div>
      ) : null}
    </>
  );
}
