import { useState } from "react";
import { useNavigate } from "react-router";
import { Copy, Eye } from "lucide-react";
import type { Campaign, EmailContent, SmsContent } from "../../api";
import { useCampaignReport, useDuplicateCampaign, useSession } from "../../api";
import { TimeSeriesChart } from "../../components/charts/TimeSeriesChart";
import { EmailPreview } from "../../components/email/EmailPreview";
import { PhonePreview } from "../../components/email/SmsEditor";
import { Button, Card, EmptyState, Modal, Page, PageHeader, Skeleton, Stat, StatusBadge, Table, Td, Th } from "../../components/ui";
import { formatCurrency, formatDateTime, formatNumber, formatPercent } from "../../lib/format";
import { ChannelIcon } from "./CampaignsPage";

export function CampaignReportView({ campaign }: { campaign: Campaign }) {
  const { account } = useSession();
  const report = useCampaignReport(campaign.id, true, campaign.status === "sending");
  const duplicate = useDuplicateCampaign();
  const navigate = useNavigate();
  const [viewing, setViewing] = useState(false);
  const stats = report.data?.stats ?? campaign.stats;
  const loading = report.isPending;
  const tz = account.timezone;
  const totalClicks = (report.data?.links ?? []).reduce((sum, l) => sum + l.clicks, 0);
  const hourFmt = (v: string) =>
    new Intl.DateTimeFormat("en-US", { timeZone: tz, month: "short", day: "numeric", hour: "numeric" }).format(new Date(v));

  return (
    <Page wide>
      <PageHeader
        breadcrumbs={[{ label: "Campaigns", to: "/campaigns" }]}
        title={
          <span className="flex items-center gap-2.5">
            <ChannelIcon channel={campaign.channel} />
            {campaign.name}
            <StatusBadge status={campaign.status} />
          </span>
        }
        description={campaign.sent_at ? `Sent ${formatDateTime(campaign.sent_at, tz)}` : "Sending now. Numbers update as messages go out."}
        actions={
          <>
            <Button variant="ghost" icon={<Eye className="size-3.5" />} onClick={() => setViewing(true)}>
              View message
            </Button>
            <Button icon={<Copy className="size-3.5" />} loading={duplicate.isPending} onClick={() => duplicate.mutate(campaign.id, { onSuccess: (d) => navigate(`/campaigns/${d.id}`) })}>
              Duplicate
            </Button>
          </>
        }
      />
      <div className="grid grid-cols-2 gap-3 md:grid-cols-4">
        <Stat label="Recipients" value={formatNumber(stats.recipients)} sub={`${formatNumber(stats.delivered)} delivered`} />
        {campaign.channel === "email" ? (
          <Stat label="Open rate" value={formatPercent(stats.open_rate)} sub={`${formatNumber(stats.unique_opens)} unique opens`} />
        ) : (
          <Stat label="Delivered" value={formatNumber(stats.delivered)} sub={`${formatNumber(stats.failed)} failed`} />
        )}
        <Stat label="Click rate" value={formatPercent(stats.click_rate)} sub={`${formatNumber(stats.unique_clicks)} unique clicks`} />
        <Stat label="Revenue" value={formatCurrency(stats.revenue)} sub={`${formatNumber(stats.orders)} orders`} />
        <Stat label="Unsubscribes" value={formatNumber(stats.unsubscribes)} sub={stats.delivered ? formatPercent(stats.unsubscribes / stats.delivered, 2) : undefined} />
        <Stat label="Bounced" value={formatNumber(stats.bounced)} />
        <Stat label="Spam complaints" value={formatNumber(stats.complaints)} />
        <Stat label="Failed" value={formatNumber(stats.failed)} />
      </div>
      <div className="mt-4 grid grid-cols-1 gap-4 lg:grid-cols-[minmax(0,1.4fr)_minmax(0,1fr)]">
        <Card title={campaign.channel === "email" ? "Opens and clicks over time" : "Clicks over time"}>
          <TimeSeriesChart
            loading={loading}
            data={report.data?.series ?? []}
            xKey="hour"
            xFormat={hourFmt}
            series={campaign.channel === "email" ? [{ key: "opens", label: "Opens" }, { key: "clicks", label: "Clicks" }] : [{ key: "clicks", label: "Clicks" }]}
          />
        </Card>
        <Card title="Links" padded={false}>
          {loading ? (
            <div className="space-y-2 p-4">
              {[0, 1, 2].map((i) => (
                <Skeleton key={i} className="h-6" />
              ))}
            </div>
          ) : (report.data?.links ?? []).length === 0 ? (
            <EmptyState title="No clicks yet" />
          ) : (
            <Table className="rounded-none border-0 shadow-none">
              <thead>
                <tr>
                  <Th>URL</Th>
                  <Th className="text-right">Clicks</Th>
                  <Th className="text-right">Share</Th>
                </tr>
              </thead>
              <tbody>
                {[...(report.data?.links ?? [])]
                  .sort((a, b) => b.clicks - a.clicks)
                  .map((l) => (
                    <tr key={l.url}>
                      <Td className="max-w-[280px] truncate">
                        <a href={l.url} target="_blank" rel="noreferrer" className="text-accent-700 hover:underline" title={l.url}>
                          {l.url}
                        </a>
                      </Td>
                      <Td className="text-right tabular-nums">{formatNumber(l.clicks)}</Td>
                      <Td className="text-right tabular-nums text-zinc-500">{totalClicks ? formatPercent(l.clicks / totalClicks, 0) : "—"}</Td>
                    </tr>
                  ))}
              </tbody>
            </Table>
          )}
        </Card>
      </div>
      {report.error ? <p className="mt-3 text-sm text-red-600">{report.error.message}</p> : null}
      {viewing ? (
        <Modal title="Message" onClose={() => setViewing(false)} size="lg">
          {campaign.channel === "email" ? (
            <EmailPreview content={campaign.content as EmailContent} className="h-[70vh]" />
          ) : (
            <PhonePreview body={(campaign.content as SmsContent).body} mediaUrl={(campaign.content as SmsContent).media_url} from={account.sms_from ?? account.name} />
          )}
        </Modal>
      ) : null}
    </Page>
  );
}
