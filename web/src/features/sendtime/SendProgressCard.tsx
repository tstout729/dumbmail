import { Card, EmptyState, Skeleton, Stat, Table, Td, Th } from "../../components/ui";
import { formatDateTime, formatNumber } from "../../lib/format";
import { useSendProgress } from "./api";

const shownRows = 60;

/** The campaign report's view of when messages go out: by release time and time zone. */
export function SendProgressCard({ campaignId, live, timezone }: { campaignId: string; live: boolean; timezone: string }) {
  const progress = useSendProgress(campaignId, live);
  if (progress.isPending) return <Skeleton className="mb-4 h-32 w-full" />;
  if (progress.error) return <p className="mb-4 text-sm text-red-600">{progress.error.message}</p>;
  const p = progress.data;
  const timed = p.buckets.some((b) => b.at !== null);
  if (!timed && p.totals.skipped_smart_sending === 0) return null;
  const at = (iso: string | null) => (iso ? formatDateTime(iso, timezone) : "Right away");
  return (
    <Card title="Delivery schedule" className="mb-4">
      <p className="mb-3 text-sm text-zinc-600">{p.description}</p>
      <div className="mb-4 grid grid-cols-2 gap-3 md:grid-cols-4">
        <Stat label="Sent" value={formatNumber(p.totals.sent)} />
        <Stat label="Waiting" value={formatNumber(p.totals.waiting)} sub={p.next_release ? `Next: ${at(p.next_release)}` : undefined} />
        <Stat label="Skipped (smart sending)" value={formatNumber(p.totals.skipped_smart_sending)} />
        <Stat label="Skipped (other)" value={formatNumber(p.totals.skipped_other)} sub={p.skip_reasons.filter((r) => r.reason !== "smart sending").map((r) => `${r.reason}: ${formatNumber(r.count)}`).join(" · ") || undefined} />
      </div>
      {timed ? (
        <div className="overflow-x-auto">
          <Table>
            <thead>
              <tr>
                <Th>Goes out (your time)</Th>
                <Th>Time zones</Th>
                <Th className="text-right">Waiting</Th>
                <Th className="text-right">Sent</Th>
                <Th className="text-right">Skipped</Th>
              </tr>
            </thead>
            <tbody>
              {p.buckets.slice(0, shownRows).map((b) => (
                <tr key={b.at ?? "now"}>
                  <Td className="whitespace-nowrap tabular-nums">{at(b.at)}</Td>
                  <Td className="max-w-[240px] truncate text-zinc-600">{b.zones.length ? b.zones.map((z) => z.replace(/_/g, " ")).join(", ") : "—"}</Td>
                  <Td className="text-right tabular-nums">{formatNumber(b.waiting)}</Td>
                  <Td className="text-right tabular-nums">{formatNumber(b.sent)}</Td>
                  <Td className="text-right tabular-nums text-zinc-500">{formatNumber(b.skipped_smart_sending + b.skipped_other)}</Td>
                </tr>
              ))}
            </tbody>
          </Table>
          {p.buckets.length > shownRows ? <p className="mt-2 text-xs text-zinc-500">Showing the first {shownRows} release times.</p> : null}
        </div>
      ) : p.buckets.length === 0 ? (
        <EmptyState title="Nothing planned yet" />
      ) : null}
    </Card>
  );
}
