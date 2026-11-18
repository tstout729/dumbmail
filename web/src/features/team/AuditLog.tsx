import { useState, type FormEvent } from "react";
import { Download, ScrollText } from "lucide-react";
import { errorMessage } from "../../api";
import { Button, buttonClass, Card, EmptyRow, EmptyState, InlineError, Input, Select, SkeletonRows, Table, Td, Th } from "../../components/ui";
import { formatNumber } from "../../lib/format";
import { auditExportHref, useAudit } from "./api";
import type { AuditEntry, AuditFilters } from "./types";

const actionGroups: { value: string; label: string }[] = [
  { value: "", label: "Everything" },
  { value: "login", label: "Sign-ins" },
  { value: "login.failed", label: "Failed sign-ins" },
  { value: "team.", label: "Team and roles" },
  { value: "api_key.", label: "API keys" },
  { value: "campaign.", label: "Campaigns and sends" },
  { value: "flow.", label: "Flows" },
  { value: "mcp.", label: "Changes through MCP" },
  { value: "account.", label: "Account settings" },
  { value: "billing.", label: "Billing" },
  { value: "domain.", label: "Domains" },
  { value: "two_factor.", label: "Two-factor" },
  { value: "sso.", label: "Single sign-on" },
  { value: "password.", label: "Passwords" },
];

const PAGE = 50;

function describe(e: AuditEntry): string {
  const parts = Object.entries(e.details)
    .filter(([k, v]) => k !== "via" && v !== null && v !== "")
    .map(([k, v]) => `${k.replace(/_/g, " ")}: ${typeof v === "string" ? v : JSON.stringify(v)}`);
  return parts.join(" · ");
}

/** Who did what and when, with filters and a CSV download. */
export function AuditLogCard() {
  const [filters, setFilters] = useState<AuditFilters>({});
  const audit = useAudit(filters);
  const rows = audit.data?.data ?? [];
  const total = audit.data?.total ?? 0;
  const offset = filters.offset ?? 0;

  const onSubmit = (e: FormEvent<HTMLFormElement>) => {
    e.preventDefault();
    const d = new FormData(e.currentTarget);
    const text = (k: string) => String(d.get(k) ?? "").trim() || undefined;
    setFilters({ action: text("action"), q: text("q"), from: text("from"), to: text("to") });
  };

  return (
    <Card
      title="Activity log"
      padded={false}
      actions={
        <a href={auditExportHref(filters)} className={buttonClass("secondary", "sm")} download>
          <Download className="size-3.5" />
          Download CSV
        </a>
      }
    >
      <form onSubmit={onSubmit} className="grid gap-2 border-b border-zinc-100 p-3 sm:grid-cols-[minmax(0,1fr)_minmax(0,1fr)_auto_auto_auto]">
        <Select name="action" aria-label="What happened" defaultValue="">
          {actionGroups.map((g) => (
            <option key={g.value} value={g.value}>
              {g.label}
            </option>
          ))}
        </Select>
        <Input name="q" placeholder="Search person, key or target" aria-label="Search" />
        <Input name="from" type="date" aria-label="From" className="sm:w-36" />
        <Input name="to" type="date" aria-label="To" className="sm:w-36" />
        <Button type="submit">Filter</Button>
      </form>
      {audit.error ? (
        <div className="p-3">
          <InlineError message={errorMessage(audit.error)} />
        </div>
      ) : null}
      <div className="overflow-x-auto">
        <Table className="rounded-none border-0 shadow-none">
          <thead>
            <tr>
              <Th>What</Th>
              <Th>Who</Th>
              <Th className="hidden sm:table-cell">When</Th>
              <Th className="hidden md:table-cell">Details</Th>
              <Th className="hidden lg:table-cell">Address</Th>
            </tr>
          </thead>
          <tbody>
            {audit.isPending ? (
              <SkeletonRows cols={5} rows={4} />
            ) : rows.length === 0 ? (
              <EmptyRow cols={5}>
                <EmptyState icon={<ScrollText className="size-5" />} title="Nothing here" description="Sign-ins, team changes, sends and settings changes show up here." />
              </EmptyRow>
            ) : (
              rows.map((e) => (
                <tr key={e.id}>
                  <Td>
                    <code className="font-mono text-xs text-zinc-800">{e.action}</code>
                    {e.target ? <div className="max-w-56 truncate text-xs text-zinc-500">{e.target}</div> : null}
                    <div className="text-xs text-zinc-400 sm:hidden">{e.local_time}</div>
                  </Td>
                  <Td>
                    <div className="max-w-48 truncate text-zinc-900">{e.actor.label ?? (e.actor.type === "system" ? "DumbMail" : "Someone")}</div>
                    {e.actor.type === "api_key" ? <div className="text-xs text-zinc-500">API key</div> : null}
                  </Td>
                  <Td className="hidden whitespace-nowrap text-xs text-zinc-600 sm:table-cell">{e.local_time}</Td>
                  <Td className="hidden max-w-72 truncate text-xs text-zinc-500 md:table-cell">{describe(e)}</Td>
                  <Td className="hidden font-mono text-xs text-zinc-500 lg:table-cell">{e.ip ?? "—"}</Td>
                </tr>
              ))
            )}
          </tbody>
        </Table>
      </div>
      {total > PAGE ? (
        <div className="flex items-center justify-between gap-2 border-t border-zinc-100 px-4 py-2 text-xs text-zinc-500">
          <span>
            {formatNumber(offset + 1)}–{formatNumber(Math.min(offset + PAGE, total))} of {formatNumber(total)}
          </span>
          <div className="flex gap-2">
            <Button size="sm" disabled={offset === 0} onClick={() => setFilters({ ...filters, offset: Math.max(0, offset - PAGE) })}>
              Newer
            </Button>
            <Button size="sm" disabled={offset + PAGE >= total} onClick={() => setFilters({ ...filters, offset: offset + PAGE })}>
              Older
            </Button>
          </div>
        </div>
      ) : null}
      <p className="border-t border-zinc-100 px-4 py-2 text-xs text-zinc-500">Kept for 13 months. Never includes passwords, keys or message content.</p>
    </Card>
  );
}
