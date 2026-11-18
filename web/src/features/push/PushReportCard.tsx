import { Card, Skeleton, Table, Td, Th } from "../../components/ui";
import { formatNumber } from "../../lib/format";
import { usePushReport } from "./api";

const LABELS: Record<string, string> = { web: "Browsers", ios: "iOS", android: "Android" };

/** Push results by platform for a campaign or flow. Delivery is only known for web push. */
export function PushReportCard({ campaignId, flowId }: { campaignId?: string; flowId?: string }) {
  const report = usePushReport({ campaign_id: campaignId, flow_id: flowId });
  return (
    <Card title="Devices" padded={false}>
      {report.isPending ? (
        <div className="p-4">
          <Skeleton className="h-16 w-full" />
        </div>
      ) : report.error ? (
        <p className="p-4 text-sm text-red-600">{report.error.message}</p>
      ) : report.data.platforms.length === 0 ? (
        <p className="p-4 text-sm text-zinc-500">No devices reached yet.</p>
      ) : (
        <div className="overflow-x-auto">
          <Table className="rounded-none border-0 shadow-none">
            <thead>
              <tr>
                <Th>Platform</Th>
                <Th className="text-right">Sent</Th>
                <Th className="text-right">Delivered</Th>
                <Th className="text-right">Clicked</Th>
                <Th className="text-right">Failed</Th>
                <Th className="text-right">Removed</Th>
              </tr>
            </thead>
            <tbody>
              {report.data.platforms.map((p) => (
                <tr key={p.platform}>
                  <Td>{LABELS[p.platform] ?? p.platform}</Td>
                  <Td className="text-right tabular-nums">{formatNumber(p.sent)}</Td>
                  <Td className="text-right tabular-nums" title={p.delivered === null ? "Apple and Google don't report delivery" : undefined}>
                    {p.delivered === null ? "—" : formatNumber(p.delivered)}
                  </Td>
                  <Td className="text-right tabular-nums">{formatNumber(p.clicked)}</Td>
                  <Td className="text-right tabular-nums">{formatNumber(p.failed + p.retrying)}</Td>
                  <Td className="text-right tabular-nums">{formatNumber(p.removed)}</Td>
                </tr>
              ))}
            </tbody>
          </Table>
          <p className="border-t border-zinc-100 px-4 py-2 text-xs text-zinc-500">
            Delivered counts browsers that showed it; apps don't report that. Removed devices had turned notifications off or uninstalled.
          </p>
        </div>
      )}
    </Card>
  );
}
