import { Link } from "react-router";
import { BarChart3, CalendarClock, Plus } from "lucide-react";
import { errorMessage, useSession } from "../../api";
import { ButtonLink, Card, EmptyRow, EmptyState, Page, PageHeader, RowLink, SkeletonRows, Table, Td, Th } from "../../components/ui";
import { formatDateTime } from "../../lib/format";
import { useCatalog, useReportList } from "./api";

export function ReportsPage() {
  const list = useReportList();
  const catalog = useCatalog();
  const { account } = useSession();
  const sourceLabel = (key: string) => catalog.data?.sources.find((s) => s.key === key)?.label ?? key;
  const saved = list.data?.saved ?? [];
  return (
    <Page>
      <PageHeader
        title="Reports"
        description="Ready-made reports, and your own built from campaigns, flows, revenue, forms, lists and metrics."
        actions={
          <ButtonLink to="/analytics/reports/new" variant="primary" icon={<Plus className="size-4" />}>
            New report
          </ButtonLink>
        }
      />
      {list.error ? <p className="mb-4 text-sm text-red-600">{errorMessage(list.error)}</p> : null}
      <h2 className="mb-2 text-sm font-semibold text-zinc-900">Ready-made</h2>
      <div className="mb-8 grid grid-cols-1 gap-3 sm:grid-cols-2 lg:grid-cols-4">
        {(list.data?.builtin ?? Array.from({ length: 8 }, () => null)).map((b, i) =>
          b ? (
            <Link key={b.id} to={`/analytics/reports/${b.id}`} className="group block">
              <Card className="h-full transition-colors group-hover:border-zinc-300">
                <div className="text-sm font-semibold text-zinc-900 group-hover:text-accent-800">{b.name}</div>
                <p className="mt-1 text-xs leading-relaxed text-zinc-500">{b.description}</p>
              </Card>
            </Link>
          ) : (
            <Card key={i} className="h-28">
              <div className="pl-skeleton h-4 w-32 rounded" />
            </Card>
          ),
        )}
      </div>
      <h2 className="mb-2 text-sm font-semibold text-zinc-900">Saved</h2>
      <Table>
        <thead>
          <tr>
            <Th>Name</Th>
            <Th>Reports on</Th>
            <Th>Email</Th>
            <Th>Updated</Th>
          </tr>
        </thead>
        <tbody>
          {list.isPending ? (
            <SkeletonRows cols={4} rows={3} />
          ) : saved.length === 0 ? (
            <EmptyRow cols={4}>
              <EmptyState
                icon={<BarChart3 className="size-5" />}
                title="No saved reports yet"
                description="Build one from scratch, or open a ready-made report and save a copy to change it."
                action={
                  <ButtonLink to="/analytics/reports/new" icon={<Plus className="size-4" />}>
                    New report
                  </ButtonLink>
                }
              />
            </EmptyRow>
          ) : (
            saved.map((r) => (
              <RowLink key={r.id} to={`/analytics/reports/${r.id}`}>
                <Td>
                  <div className="font-medium text-zinc-900">{r.name}</div>
                  {r.description ? <div className="max-w-md truncate text-xs text-zinc-500">{r.description}</div> : null}
                </Td>
                <Td className="text-zinc-600">{sourceLabel(r.spec.source)}</Td>
                <Td className="text-zinc-600">
                  {r.schedule ? (
                    <span className="inline-flex items-center gap-1">
                      <CalendarClock className="size-3.5" />
                      {r.schedule === "weekly" ? "Mondays" : "Monthly"}
                    </span>
                  ) : (
                    "—"
                  )}
                </Td>
                <Td className="text-zinc-500">{formatDateTime(r.updated_at, account.timezone)}</Td>
              </RowLink>
            ))
          )}
        </tbody>
      </Table>
    </Page>
  );
}
