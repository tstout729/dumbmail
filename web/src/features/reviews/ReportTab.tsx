import { useState } from "react";
import { RangePicker, type RangeDays } from "../../components/charts/RangePicker";
import { TimeSeriesChart } from "../../components/charts/TimeSeriesChart";
import { Card, EmptyRow, EmptyState, Stat, Table, Td, Th } from "../../components/ui";
import { formatNumber, formatPercent } from "../../lib/format";
import { useReviewReport } from "./api";
import { Stars } from "./parts";

export function ReportTab() {
  const [days, setDays] = useState<RangeDays>(90);
  const report = useReviewReport(days);
  const r = report.data;
  return (
    <div className="space-y-4">
      <div className="flex justify-end">
        <RangePicker value={days} onChange={setDays} />
      </div>
      {report.error ? (
        <Card>
          <EmptyState title="Couldn't load the report" description={report.error.message} />
        </Card>
      ) : (
        <>
          <div className="grid gap-3 sm:grid-cols-2 lg:grid-cols-4">
            <Stat label="Requests sent" value={formatNumber(r?.requests_sent)} loading={report.isPending} />
            <Stat label="Response rate" value={r?.response_rate != null ? formatPercent(r.response_rate) : "–"} sub={r ? `${formatNumber(r.requests_answered)} answered` : undefined} loading={report.isPending} />
            <Stat label="Reviews written" value={formatNumber(r?.reviews_written)} sub={r ? `${formatNumber(r.awaiting_moderation)} waiting for you` : undefined} loading={report.isPending} />
            <Stat label="Average rating" value={r?.average_rating != null ? r.average_rating.toFixed(2) : "–"} sub="Every review written, not only published ones" loading={report.isPending} />
          </div>
          <Card title="Average rating by day">
            <TimeSeriesChart
              data={(r?.series ?? []).map((s) => ({ date: s.date, average: s.average_rating ?? 0, reviews: s.reviews }))}
              xKey="date"
              series={[{ key: "average", label: "Average rating" }]}
              format={(n) => n.toFixed(1)}
              allowDecimals
              loading={report.isPending}
            />
          </Card>
          <Table>
            <thead>
              <tr>
                <Th>Product</Th>
                <Th>Average</Th>
                <Th className="hidden sm:table-cell">Reviews</Th>
                <Th className="hidden sm:table-cell">Published average</Th>
              </tr>
            </thead>
            <tbody>
              {(r?.products ?? []).length === 0 ? (
                <EmptyRow cols={4}>
                  <EmptyState title="No reviews yet" />
                </EmptyRow>
              ) : (
                (r?.products ?? []).map((p) => (
                  <tr key={p.product_id}>
                    <Td className="max-w-[12rem] truncate font-medium text-zinc-900">{p.name}</Td>
                    <Td>
                      <div className="flex items-center gap-1.5 whitespace-nowrap">
                        <Stars value={p.average_rating} />
                        <span className="tabular-nums">{p.average_rating != null ? p.average_rating.toFixed(2) : "–"}</span>
                      </div>
                    </Td>
                    <Td className="hidden tabular-nums sm:table-cell">{formatNumber(p.reviews)}</Td>
                    <Td className="hidden tabular-nums sm:table-cell">{p.published_average != null ? `${p.published_average.toFixed(2)} (${formatNumber(p.published)})` : "–"}</Td>
                  </tr>
                ))
              )}
            </tbody>
          </Table>
        </>
      )}
    </div>
  );
}
