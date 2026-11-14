import type { AnalyticsOverview } from "../../api";
import { formatCompact, formatCurrency, formatNumber, formatPercent } from "../../lib/format";
import { TimeSeriesChart } from "../charts/TimeSeriesChart";
import { Card, Stat } from "../ui";

type Data = AnalyticsOverview | undefined;

export function KpiRow({ data, loading, extended }: { data: Data; loading: boolean; extended?: boolean }) {
  const t = data?.totals;
  return (
    <div className={extended ? "grid grid-cols-2 gap-3 md:grid-cols-4" : "grid grid-cols-2 gap-3 md:grid-cols-5"}>
      <Stat label="Attributed revenue" value={formatCurrency(t?.attributed_revenue)} loading={loading} sub={t ? `of ${formatCurrency(t.revenue)} total` : undefined} />
      <Stat label="Emails sent" value={formatNumber(t?.emails_sent)} loading={loading} />
      <Stat label="Open rate" value={formatPercent(t?.open_rate)} loading={loading} />
      <Stat label="Click rate" value={formatPercent(t?.click_rate)} loading={loading} />
      <Stat label="SMS sent" value={formatNumber(t?.sms_sent)} loading={loading} />
      {extended ? (
        <>
          <Stat label="Campaign revenue" value={formatCurrency(t?.campaign_revenue)} loading={loading} />
          <Stat label="Flow revenue" value={formatCurrency(t?.flow_revenue)} loading={loading} />
          <Stat label="Unsubscribe rate" value={formatPercent(t?.unsubscribe_rate, 2)} loading={loading} />
          <Stat label="Bounce rate" value={formatPercent(t?.bounce_rate, 2)} loading={loading} />
          <Stat label="Profiles" value={formatNumber(t?.profiles)} loading={loading} sub={t ? `+${formatNumber(t.new_profiles)} new` : undefined} />
        </>
      ) : null}
    </div>
  );
}

export function RevenueChart({ data, loading }: { data: Data; loading: boolean }) {
  return (
    <Card title="Revenue">
      <TimeSeriesChart
        loading={loading}
        data={data?.series ?? []}
        xKey="date"
        format={(n) => formatCurrency(n)}
        series={[
          { key: "attributed_revenue", label: "Attributed" },
          { key: "revenue", label: "Total" },
        ]}
      />
    </Card>
  );
}

export function SendsChart({ data, loading }: { data: Data; loading: boolean }) {
  return (
    <Card title="Messages sent">
      <TimeSeriesChart
        loading={loading}
        data={data?.series ?? []}
        xKey="date"
        format={(n) => formatCompact(n)}
        series={[
          { key: "emails_sent", label: "Email" },
          { key: "sms_sent", label: "SMS" },
        ]}
      />
    </Card>
  );
}

export function EngagementChart({ data, loading }: { data: Data; loading: boolean }) {
  return (
    <Card title="Opens and clicks">
      <TimeSeriesChart
        loading={loading}
        data={data?.series ?? []}
        xKey="date"
        format={(n) => formatCompact(n)}
        series={[
          { key: "opens", label: "Opens" },
          { key: "clicks", label: "Clicks" },
        ]}
      />
    </Card>
  );
}
