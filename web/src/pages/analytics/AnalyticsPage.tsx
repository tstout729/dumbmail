import { Link, useSearchParams } from "react-router";
import { useOverview, useSession } from "../../api";
import { EngagementChart, KpiRow, RevenueChart, SendsChart } from "../../components/analytics/OverviewParts";
import { RangePicker, parseRange } from "../../components/charts/RangePicker";
import { Card, EmptyRow, Page, PageHeader, SkeletonRows, StatusBadge, Table, Td, Th } from "../../components/ui";
import { formatCurrency, formatDate, formatNumber, formatPercent } from "../../lib/format";

export function AnalyticsPage() {
  const { account } = useSession();
  const [params, setParams] = useSearchParams();
  const days = parseRange(params.get("days"));
  const overview = useOverview(days);
  const data = overview.data;
  const loading = overview.isPending;
  const t = data?.totals;
  const attributed = (t?.campaign_revenue ?? 0) + (t?.flow_revenue ?? 0);

  return (
    <Page>
      <PageHeader
        title="Analytics"
        description={`Last ${days} days. Revenue is credited to the newest email opened or clicked in the prior 5 days, or SMS clicked in the prior 24 hours.`}
        actions={<RangePicker value={days} onChange={(d) => setParams({ days: String(d) }, { replace: true })} />}
      />
      {overview.error ? <p className="mb-4 text-sm text-red-600">{overview.error.message}</p> : null}
      <KpiRow data={data} loading={loading} extended />
      <div className="mt-4 grid grid-cols-1 gap-4 lg:grid-cols-2">
        <RevenueChart data={data} loading={loading} />
        <Card title="Attributed revenue by source">
          <div className="space-y-4 pt-1">
            {(
              [
                ["Flows", t?.flow_revenue ?? 0, "#219464"],
                ["Campaigns", t?.campaign_revenue ?? 0, "#6d5bd0"],
              ] as const
            ).map(([label, value, color]) => (
              <div key={label}>
                <div className="mb-1 flex justify-between text-sm">
                  <span className="flex items-center gap-2 text-zinc-700">
                    <span className="size-2 rounded-full" style={{ background: color }} />
                    {label}
                  </span>
                  <span className="font-medium tabular-nums text-zinc-900">
                    {formatCurrency(value)} <span className="text-xs font-normal text-zinc-500">{attributed ? formatPercent(value / attributed, 0) : ""}</span>
                  </span>
                </div>
                <div className="h-2 overflow-hidden rounded-full bg-zinc-100">
                  <div className="h-full rounded-full" style={{ width: `${attributed ? (value / attributed) * 100 : 0}%`, background: color }} />
                </div>
              </div>
            ))}
            <p className="text-xs text-zinc-500">
              {formatCurrency(t?.attributed_revenue)} of {formatCurrency(t?.revenue)} total revenue came from DumbMail messages.
            </p>
          </div>
        </Card>
        <SendsChart data={data} loading={loading} />
        <EngagementChart data={data} loading={loading} />
      </div>
      <div className="mt-4 grid grid-cols-1 gap-4 xl:grid-cols-2">
        <div>
          <h2 className="mb-2 text-sm font-semibold text-zinc-900">Campaigns</h2>
          <Table>
            <thead>
              <tr>
                <Th>Campaign</Th>
                <Th className="text-right">Recipients</Th>
                <Th className="text-right">Open</Th>
                <Th className="text-right">Click</Th>
                <Th className="text-right">Revenue</Th>
              </tr>
            </thead>
            <tbody>
              {loading ? (
                <SkeletonRows cols={5} rows={4} />
              ) : (data?.top_campaigns ?? []).length === 0 ? (
                <EmptyRow cols={5}>
                  <p className="px-4 py-6 text-center text-xs text-zinc-500">No campaigns sent in this range.</p>
                </EmptyRow>
              ) : (
                (data?.top_campaigns ?? []).map((c) => (
                  <tr key={c.id} className="hover:bg-zinc-50">
                    <Td>
                      <Link to={`/campaigns/${c.id}`} className="font-medium text-zinc-900 hover:text-accent-700">
                        {c.name}
                      </Link>
                      <div className="text-xs text-zinc-500">{formatDate(c.sent_at, account.timezone)}</div>
                    </Td>
                    <Td className="text-right tabular-nums">{formatNumber(c.recipients)}</Td>
                    <Td className="text-right tabular-nums">{formatPercent(c.open_rate)}</Td>
                    <Td className="text-right tabular-nums">{formatPercent(c.click_rate)}</Td>
                    <Td className="text-right font-medium tabular-nums">{formatCurrency(c.revenue)}</Td>
                  </tr>
                ))
              )}
            </tbody>
          </Table>
        </div>
        <div>
          <h2 className="mb-2 text-sm font-semibold text-zinc-900">Flows</h2>
          <Table>
            <thead>
              <tr>
                <Th>Flow</Th>
                <Th>Status</Th>
                <Th className="text-right">Recipients</Th>
                <Th className="text-right">Revenue</Th>
              </tr>
            </thead>
            <tbody>
              {loading ? (
                <SkeletonRows cols={4} rows={4} />
              ) : (data?.top_flows ?? []).length === 0 ? (
                <EmptyRow cols={4}>
                  <p className="px-4 py-6 text-center text-xs text-zinc-500">No flow activity in this range.</p>
                </EmptyRow>
              ) : (
                (data?.top_flows ?? []).map((f) => (
                  <tr key={f.id} className="hover:bg-zinc-50">
                    <Td>
                      <Link to={`/flows/${f.id}`} className="font-medium text-zinc-900 hover:text-accent-700">
                        {f.name}
                      </Link>
                    </Td>
                    <Td>
                      <StatusBadge status={f.status} />
                    </Td>
                    <Td className="text-right tabular-nums">{formatNumber(f.recipients)}</Td>
                    <Td className="text-right font-medium tabular-nums">{formatCurrency(f.revenue)}</Td>
                  </tr>
                ))
              )}
            </tbody>
          </Table>
        </div>
      </div>
    </Page>
  );
}
