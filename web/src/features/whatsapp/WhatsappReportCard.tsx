import { Card, Skeleton, Stat } from "../../components/ui";
import { formatNumber } from "../../lib/format";
import { useWhatsappReport } from "./api";

/** WhatsApp results for a campaign or flow: sent, delivered, read, clicked and failed, with the top reasons. */
export function WhatsappReportCard({ campaignId, flowId }: { campaignId?: string; flowId?: string }) {
  const report = useWhatsappReport({ campaign_id: campaignId, flow_id: flowId });
  return (
    <Card title="WhatsApp">
      {report.isPending ? (
        <Skeleton className="h-16 w-full" />
      ) : report.error ? (
        <p className="text-sm text-red-600">{report.error.message}</p>
      ) : (
        <div className="space-y-3">
          <div className="grid grid-cols-2 gap-3 sm:grid-cols-5">
            <Stat label="Sent" value={formatNumber(report.data.sent)} />
            <Stat label="Delivered" value={formatNumber(report.data.delivered)} />
            <Stat label="Read" value={formatNumber(report.data.read)} />
            <Stat label="Clicked" value={formatNumber(report.data.clicked)} />
            <Stat label="Failed" value={formatNumber(report.data.failed)} sub={report.data.skipped ? `${formatNumber(report.data.skipped)} skipped` : undefined} />
          </div>
          {report.data.reasons.length > 0 ? (
            <ul className="space-y-0.5 text-xs text-zinc-500">
              {report.data.reasons.map((r) => (
                <li key={r.reason ?? ""}>
                  {formatNumber(r.count)} · {r.reason ?? "no reason given"}
                </li>
              ))}
            </ul>
          ) : null}
          <p className="text-xs text-zinc-500">Read counts people whose WhatsApp sent a read receipt; people who turned read receipts off show as delivered.</p>
        </div>
      )}
    </Card>
  );
}
