import { Card, Skeleton, Stat } from "../../components/ui";
import { formatNumber } from "../../lib/format";
import { useRcsReport } from "./api";

/** RCS and SMS side by side for an SMS campaign or a flow. Shown only once something went as RCS. */
export function RcsReportCard({ campaignId, flowId }: { campaignId?: string; flowId?: string }) {
  const report = useRcsReport({ campaign_id: campaignId, flow_id: flowId });
  if (report.isPending) return <Skeleton className="h-24 w-full" />;
  if (report.error || !report.data.any_rcs) return null;
  const { rcs, sms } = report.data;
  return (
    <Card title="RCS and SMS">
      <div className="space-y-3">
        <div>
          <div className="mb-1.5 text-xs font-medium text-zinc-600">As RCS</div>
          <div className="grid grid-cols-2 gap-3 sm:grid-cols-4">
            <Stat label="Sent" value={formatNumber(rcs.sent)} />
            <Stat label="Delivered" value={formatNumber(rcs.delivered)} />
            <Stat label="Read (RCS)" value={formatNumber(rcs.read ?? 0)} />
            <Stat label="Clicked" value={formatNumber(rcs.clicked)} />
          </div>
        </div>
        <div>
          <div className="mb-1.5 text-xs font-medium text-zinc-600">As SMS</div>
          <div className="grid grid-cols-2 gap-3 sm:grid-cols-4">
            <Stat label="Sent" value={formatNumber(sms.sent)} />
            <Stat label="Delivered" value={formatNumber(sms.delivered)} />
            <Stat label="Clicked" value={formatNumber(sms.clicked)} />
          </div>
        </div>
        <p className="text-xs text-zinc-500">Texts that fell back from RCS count as SMS. Read counts phones that sent a read receipt, so it undercounts.</p>
      </div>
    </Card>
  );
}
