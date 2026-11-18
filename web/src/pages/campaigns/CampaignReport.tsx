import { PushReportCard } from "../../features/push/PushReportCard";
import { PushPreview } from "../../features/push/PushEditor";
import type { PushContent } from "../../features/push/types";
import { useState, type ReactNode } from "react";
import { useNavigate } from "react-router";
import { Copy, Eye, FlaskConical, Trophy } from "lucide-react";
import type { AbMetric, Campaign, CampaignAbReport, EmailContent, MessageStats, SmsContent, VersionId } from "../../api";
import { useCampaignReport, useCancelCampaign, useDuplicateCampaign, useEstimate, usePickWinner, useSession } from "../../api";
import { TimeSeriesChart } from "../../components/charts/TimeSeriesChart";
import { EmailPreview } from "../../components/email/EmailPreview";
import { PhonePreview } from "../../components/email/SmsEditor";
import { Badge, Button, Card, ConfirmDialog, EmptyState, Modal, Notice, Page, PageHeader, Segmented, Skeleton, Stat, StatusBadge, Table, Td, Th, cx } from "../../components/ui";
import { formatCurrency, formatDateTime, formatNumber, formatPercent, pluralize } from "../../lib/format";
import { toast } from "../../lib/toast";
import { contentSnippet, metricLabels, versionContent, versionIds, versionLabel, versionName } from "./abTest";
import { ChannelIcon } from "./CampaignsPage";
import { SendProgressCard } from "../../features/sendtime/SendProgressCard";

export function CampaignReportView({ campaign }: { campaign: Campaign }) {
  const { account } = useSession();
  const report = useCampaignReport(campaign.id, true, campaign.status);
  const duplicate = useDuplicateCampaign();
  const cancel = useCancelCampaign(campaign.id);
  const navigate = useNavigate();
  const [viewing, setViewing] = useState<VersionId | null>(null);
  const [confirmCancel, setConfirmCancel] = useState(false);
  const stats = report.data?.stats ?? campaign.stats;
  const loading = report.isPending;
  const tz = account.timezone;
  const totalClicks = (report.data?.links ?? []).reduce((sum, l) => sum + l.clicks, 0);
  const hourFmt = (v: string) =>
    new Intl.DateTimeFormat("en-US", { timeZone: tz, month: "short", day: "numeric", hour: "numeric" }).format(new Date(v));
  const ab = report.data?.ab_test ?? null;
  const viewVersions = versionIds(campaign.ab_test);
  const viewed = viewing ? versionContent(campaign, viewing) : null;
  const description =
    campaign.status === "testing"
      ? `Test sent ${formatDateTime(campaign.sent_at, tz)}. Numbers update as results come in.`
      : campaign.status === "sending"
        ? "Sending now. Numbers update as messages go out."
        : `Sent ${formatDateTime(campaign.sent_at, tz)}`;

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
        description={description}
        actions={
          <>
            <Button variant="ghost" icon={<Eye className="size-3.5" />} onClick={() => setViewing("a")}>
              {viewVersions.length > 1 ? "View versions" : "View message"}
            </Button>
            {campaign.status === "testing" || campaign.status === "sending" ? (
              <Button variant="secondary" onClick={() => setConfirmCancel(true)}>
                {campaign.status === "testing" ? "Cancel the rest" : "Stop sending"}
              </Button>
            ) : null}
            <Button icon={<Copy className="size-3.5" />} loading={duplicate.isPending} onClick={() => duplicate.mutate(campaign.id, { onSuccess: (d) => navigate(`/campaigns/${d.id}`) })}>
              Duplicate
            </Button>
          </>
        }
      />
      {campaign.status === "sending" && campaign.error ? (
        <div className="mb-4">
          <Notice tone="amber" title="Paused">
            {campaign.error}
          </Notice>
        </div>
      ) : null}
      {ab ? <AbTestResults campaign={campaign} ab={ab} totalRecipients={stats.recipients} timezone={tz} /> : null}
      <SendProgressCard campaignId={campaign.id} live={campaign.status === "sending"} timezone={tz} />
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
      {campaign.channel === "push" ? (
        <div className="mt-4">
          <PushReportCard campaignId={campaign.id} />
        </div>
      ) : null}
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
      {viewing && viewed ? (
        <Modal title={viewVersions.length > 1 ? versionLabel(viewing) : "Message"} onClose={() => setViewing(null)} size="lg">
          {viewVersions.length > 1 ? (
            <div className="mb-3">
              <Segmented size="sm" value={viewing} onChange={setViewing} options={viewVersions.map((v) => ({ value: v, label: versionLabel(v) }))} />
            </div>
          ) : null}
          {campaign.channel === "email" ? (
            <EmailPreview key={viewing} content={viewed as EmailContent} className="h-[70vh]" />
          ) : campaign.channel === "push" ? (
            <PushPreview content={viewed as PushContent} rendered={null} platforms={(viewed as PushContent).platforms?.length ? ((viewed as PushContent).platforms ?? []) : ["web", "ios", "android"]} />
          ) : (
            <PhonePreview body={(viewed as SmsContent).body} mediaUrl={(viewed as SmsContent).media_url} from={account.sms_from ?? account.name} />
          )}
        </Modal>
      ) : null}
      {confirmCancel ? (
        <ConfirmDialog
          title={campaign.status === "testing" ? "Cancel the rest of this campaign?" : "Stop sending this campaign?"}
          body={
            campaign.status === "testing"
              ? "Nobody else gets it. The people in the test already got their version."
              : "Nobody who hasn't got it yet will get it, including time zones still waiting. People who already got it keep it."
          }
          confirmLabel="Cancel campaign"
          danger
          loading={cancel.isPending}
          onConfirm={() =>
            cancel.mutate(undefined, {
              onSuccess: () => {
                setConfirmCancel(false);
                toast.success("Campaign cancelled");
              },
            })
          }
          onClose={() => setConfirmCancel(false)}
        />
      ) : null}
    </Page>
  );
}

/** The count behind each metric: opens, clicks or orders. */
const metricHits: Record<AbMetric, (s: MessageStats) => number> = {
  open_rate: (s) => s.unique_opens,
  click_rate: (s) => s.unique_clicks,
  revenue: (s) => s.orders,
};
const hitWords: Record<AbMetric, string> = { open_rate: "opens", click_rate: "clicks", revenue: "orders" };

function confidenceNote(confidence: number, final: boolean) {
  if (confidence >= 0.95) return "That's a clear result.";
  if (confidence >= 0.9) return "That's a fairly clear result.";
  return final
    ? "Under 90%, the difference may be chance."
    : "Under 90%, the difference may be chance. Waiting longer usually makes it clearer.";
}

type AbResultsProps = { campaign: Campaign; ab: CampaignAbReport; totalRecipients: number; timezone: string };

/** Per-version results, who's leading, and (while testing) the winner buttons. */
function AbTestResults({ campaign, ab, totalRecipients, timezone }: AbResultsProps) {
  const pick = usePickWinner(campaign.id);
  const [picking, setPicking] = useState<VersionId | null>(null);
  // Read once when the page opens, to tell whether the test's wait is over (polling refreshes the data, not this).
  const [openedAt] = useState(() => Date.now());
  const testing = campaign.status === "testing";
  const final = campaign.status === "sent";
  // While testing, "everyone else" is whoever in the audience hasn't had the test yet.
  const estimate = useEstimate(testing ? { channel: campaign.channel, audience: campaign.audience } : null);
  const email = campaign.channel === "email";
  const metric = metricLabels[ab.metric].toLowerCase();
  const inTest = ab.versions.reduce((sum, v) => sum + v.stats.recipients, 0);
  const remaining = testing ? (estimate.data ? Math.max(0, estimate.data.count - inTest) : undefined) : Math.max(0, totalRecipients - inTest);
  const hasSignal = ab.versions.some((v) => metricHits[ab.metric](v.stats) > 0);
  const highlight = ab.winner ?? (hasSignal ? ab.leader : null);
  const endsAt = ab.ends_at ? `${formatDateTime(ab.ends_at, timezone)}` : null;
  const waitOver = ab.ends_at !== null && new Date(ab.ends_at).getTime() <= openedAt;
  const others = remaining !== undefined ? `the other ${pluralize(remaining, "person", "people")}` : "everyone else";

  const status = (() => {
    if (ab.winner) {
      return (
        <Notice tone="green" icon={<Trophy />} title={`Winner: ${versionLabel(ab.winner)}`}>
          {campaign.status === "sending" ? `Sending it to the remaining ${pluralize(remaining ?? 0, "person", "people")} now.` : `Sent to the remaining ${pluralize(remaining ?? 0, "person", "people")}.`}
        </Notice>
      );
    }
    if (testing && ab.pick === "manual") {
      return (
        <Notice tone="amber" icon={<FlaskConical />} title="Waiting for you to pick a winner">
          {waitOver ? `The test's wait ended ${endsAt}. ` : endsAt ? `The test's wait ends ${endsAt}; you can pick sooner. ` : null}
          Pick a version below to send it to {others}.
        </Notice>
      );
    }
    if (testing) {
      return (
        <Notice tone="blue" icon={<FlaskConical />} title={endsAt ? `Testing — winner picked automatically at ${endsAt}` : "Testing — winner picked automatically"}>
          The version with the best {metric} then goes to {others}. You can pick one sooner below.
        </Notice>
      );
    }
    if (campaign.status === "sending") {
      return (
        <Notice tone="blue" icon={<FlaskConical />} title="Sending the test">
          Each version goes to its share of the test group. Results come in as people respond.
        </Notice>
      );
    }
    if (ab.test_percent >= 100) {
      return (
        <Notice tone="gray" icon={<FlaskConical />} title="Everyone got one of the versions">
          The whole audience was in the test, so there was no winner send.
        </Notice>
      );
    }
    return null;
  })();

  return (
    <Card
      className="mb-4"
      title={
        <span className="flex items-center gap-2">
          A/B test <Badge tone="violet">{ab.versions.length} versions</Badge>
          <span className="font-normal text-zinc-500">· winner by {metric}</span>
        </span>
      }
      padded={false}
    >
      {status ? <div className="px-4 pt-4">{status}</div> : null}
      <Table className={cx("rounded-none border-0 shadow-none", status && "mt-3")}>
          <thead>
            <tr>
              <Th>Version</Th>
              <Th>{email ? "Subject line" : "Message"}</Th>
              <Th className="text-right">Recipients</Th>
              {email ? <MetricTh active={ab.metric === "open_rate"}>Open rate</MetricTh> : null}
              <MetricTh active={ab.metric === "click_rate"}>Click rate</MetricTh>
              <MetricTh active={ab.metric === "revenue"}>Revenue</MetricTh>
              {testing ? (
                <Th>
                  <span className="sr-only">Actions</span>
                </Th>
              ) : null}
            </tr>
          </thead>
          <tbody>
            {ab.versions.map(({ id, stats }) => {
              const top = id === highlight;
              return (
                <tr key={id} className={cx("[&:last-child>td]:border-b-0", top ? "bg-accent-50/60" : "")}>
                  <Td>
                    <span className="flex items-center gap-2 whitespace-nowrap font-medium text-zinc-900">
                      {versionLabel(id)}
                      {top ? (
                        <Badge tone="green">
                          <Trophy className="size-3" /> {ab.winner ? "Winner" : final ? "Best" : "Leading"}
                        </Badge>
                      ) : null}
                    </span>
                  </Td>
                  <Td className="max-w-[320px] truncate text-zinc-600" title={contentSnippet(versionContent(campaign, id), campaign.channel)}>
                    {contentSnippet(versionContent(campaign, id), campaign.channel)}
                  </Td>
                  <Td className="text-right tabular-nums">{formatNumber(stats.recipients)}</Td>
                  {email ? <MetricTd active={ab.metric === "open_rate"}>{formatPercent(stats.open_rate)}</MetricTd> : null}
                  <MetricTd active={ab.metric === "click_rate"}>{formatPercent(stats.click_rate)}</MetricTd>
                  <MetricTd active={ab.metric === "revenue"}>
                    {formatCurrency(stats.revenue)}
                    {ab.metric === "revenue" ? (
                      <span className="block text-xs font-normal text-zinc-500">{formatCurrency(stats.delivered ? stats.revenue / stats.delivered : 0, true)} per person</span>
                    ) : null}
                  </MetricTd>
                  {testing ? (
                    <Td className="text-right">
                      <Button size="sm" variant={top ? "primary" : "secondary"} onClick={() => setPicking(id)}>
                        Send {versionName(id)} to everyone else
                      </Button>
                    </Td>
                  ) : null}
                </tr>
              );
            })}
          </tbody>
        </Table>
      <p className="border-t border-zinc-100 px-4 py-3 text-xs text-zinc-600">
        {hasSignal && ab.confidence === null ? (
          <>
            <span className="font-medium text-zinc-800">{versionLabel(ab.leader)} leads on {metric}.</span> One big order can swing revenue per
            person, so there's no confidence figure here. Compare the order counts too.
          </>
        ) : hasSignal ? (
          <>
            <span className="font-medium text-zinc-800">
              {formatPercent(ab.confidence ?? 0.5, 0)} confident {versionLabel(ab.leader)} beats the runner-up on {metric}.
            </span>{" "}
            {confidenceNote(ab.confidence ?? 0.5, final)}
          </>
        ) : (
          `No ${hitWords[ab.metric]} yet, so no version is ahead. Check back once people have had time to respond.`
        )}
      </p>
      {picking ? (
        <ConfirmDialog
          title={`Send ${versionName(picking)} to everyone else?`}
          body={
            <>
              {remaining !== undefined ? `It goes to about ${pluralize(remaining, "more person", "more people")} right away` : "It goes to everyone in the audience who wasn't in the test, right away"}{" "}
              and ends the test. You can't unsend it.
              {hasSignal && picking !== ab.leader ? ` ${versionLabel(ab.leader)} is leading right now.` : null}
            </>
          }
          confirmLabel={`Send ${versionName(picking)}`}
          loading={pick.isPending}
          onConfirm={() =>
            pick.mutate(picking, {
              onSuccess: () => {
                toast.success(`Sending ${versionName(picking)} to everyone else`);
                setPicking(null);
              },
              onError: () => setPicking(null),
            })
          }
          onClose={() => setPicking(null)}
        />
      ) : null}
    </Card>
  );
}

function MetricTh({ active, children }: { active: boolean; children: string }) {
  return (
    <Th className={cx("text-right", active && "text-zinc-900")} title={active ? "Picks the winner" : undefined}>
      {children}
      {active ? <span className="ml-1 text-violet-700">●</span> : null}
    </Th>
  );
}

function MetricTd({ active, children }: { active: boolean; children: ReactNode }) {
  return <Td className={cx("text-right tabular-nums", active && "font-semibold text-zinc-900")}>{children}</Td>;
}
