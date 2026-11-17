import { useState } from "react";
import { Download, FileDown } from "lucide-react";
import { errorMessage, useCampaigns, useLists, useMetrics, useSegments, useSession } from "../../api";
import { Button, Card, Checkbox, EmptyRow, EmptyState, Field, Input, Page, PageHeader, Select, SkeletonRows, StatusBadge, Table, TagInput, Td, Th } from "../../components/ui";
import { formatDateTime, formatNumber } from "../../lib/format";
import { toast } from "../../lib/toast";
import { downloadCsv, useExports, useStartExport } from "./api";
import { formatBytes } from "./format";
import type { ExportFile, ExportKind, ExportRequest } from "./types";

const COLUMNS: { key: string; label: string }[] = [
  { key: "email", label: "Email" },
  { key: "phone_number", label: "Phone number" },
  { key: "first_name", label: "First name" },
  { key: "last_name", label: "Last name" },
  { key: "email_consent", label: "Email consent" },
  { key: "sms_consent", label: "SMS consent" },
  { key: "email_suppressed_reason", label: "Email suppressed reason" },
  { key: "external_id", label: "External ID" },
  { key: "id", label: "Profile ID" },
  { key: "created_at", label: "Created" },
  { key: "updated_at", label: "Updated" },
  { key: "last_event_at", label: "Last active" },
];
const DEFAULT_COLUMNS = ["email", "phone_number", "first_name", "last_name", "email_consent", "sms_consent", "created_at"];

const KIND_LABEL: Record<ExportFile["kind"], string> = {
  profiles: "Profiles",
  events: "Events",
  campaign_recipients: "Campaign recipients",
  suppressions: "Unsubscribes and suppressions",
  report: "Scheduled report",
};

function NewExport({ busy }: { busy: boolean }) {
  const start = useStartExport();
  const lists = useLists();
  const segments = useSegments();
  const metrics = useMetrics();
  const campaigns = useCampaigns();
  const { account } = useSession();
  const [kind, setKind] = useState<ExportKind>("profiles");
  const [scope, setScope] = useState<"all" | "list" | "segment">("all");
  const [listId, setListId] = useState("");
  const [segmentId, setSegmentId] = useState("");
  const [columns, setColumns] = useState<string[]>(DEFAULT_COLUMNS);
  const [properties, setProperties] = useState<string[]>([]);
  const [metric, setMetric] = useState("");
  const [startDate, setStartDate] = useState("");
  const [endDate, setEndDate] = useState("");
  const [campaignId, setCampaignId] = useState("");
  const sent = (campaigns.data ?? []).filter((c) => c.status === "sent" || c.status === "sending" || c.status === "testing");

  const request = (): ExportRequest | null => {
    switch (kind) {
      case "profiles":
        if (scope === "list" && !listId) return null;
        if (scope === "segment" && !segmentId) return null;
        if (columns.length === 0 && properties.length === 0) return null;
        return { kind, scope, list_id: scope === "list" ? listId : undefined, segment_id: scope === "segment" ? segmentId : undefined, columns, properties };
      case "events":
        return metric ? { kind, metric, start: startDate || undefined, end: endDate || undefined } : null;
      case "campaign_recipients":
        return campaignId ? { kind, campaign_id: campaignId } : null;
      default:
        return { kind };
    }
  };
  const req = request();
  return (
    <Card title="New export">
      <div className="space-y-4">
        <Field label="What to export">
          <Select value={kind} onChange={(e) => setKind(e.target.value as ExportKind)}>
            <option value="profiles">Profiles</option>
            <option value="events">Events of a metric</option>
            <option value="campaign_recipients">A campaign's recipients and what each one did</option>
            <option value="suppressions">Unsubscribes and suppressions</option>
          </Select>
        </Field>
        {kind === "profiles" ? (
          <>
            <div className="grid grid-cols-1 gap-3 sm:grid-cols-2">
              <Field label="Who">
                <Select value={scope} onChange={(e) => setScope(e.target.value as typeof scope)}>
                  <option value="all">Everyone</option>
                  <option value="list">People on a list</option>
                  <option value="segment">People in a segment</option>
                </Select>
              </Field>
              {scope === "list" ? (
                <Field label="List">
                  <Select value={listId} onChange={(e) => setListId(e.target.value)}>
                    <option value="">Pick a list</option>
                    {(lists.data ?? []).map((l) => (
                      <option key={l.id} value={l.id}>
                        {l.name}
                      </option>
                    ))}
                  </Select>
                </Field>
              ) : scope === "segment" ? (
                <Field label="Segment">
                  <Select value={segmentId} onChange={(e) => setSegmentId(e.target.value)}>
                    <option value="">Pick a segment</option>
                    {(segments.data ?? []).map((s) => (
                      <option key={s.id} value={s.id}>
                        {s.name}
                      </option>
                    ))}
                  </Select>
                </Field>
              ) : null}
            </div>
            <Field label="Columns">
              <div className="grid grid-cols-1 gap-1.5 sm:grid-cols-3">
                {COLUMNS.map((c) => (
                  <Checkbox
                    key={c.key}
                    label={c.label}
                    checked={columns.includes(c.key)}
                    onChange={() => setColumns(columns.includes(c.key) ? columns.filter((x) => x !== c.key) : [...columns, c.key])}
                  />
                ))}
              </div>
            </Field>
            <Field label="Custom properties" hint="Each property becomes a column. Up to 50.">
              <TagInput value={properties} onChange={(v) => setProperties(v.slice(0, 50))} placeholder="e.g. birthday, then Enter" aria-label="Custom properties" />
            </Field>
          </>
        ) : null}
        {kind === "events" ? (
          <div className="grid grid-cols-1 gap-3 sm:grid-cols-3">
            <Field label="Metric">
              <Select value={metric} onChange={(e) => setMetric(e.target.value)}>
                <option value="">Pick a metric</option>
                {(metrics.data ?? []).map((m) => (
                  <option key={m.id} value={m.name}>
                    {m.name}
                  </option>
                ))}
              </Select>
            </Field>
            <Field label="From" hint={`Default: 30 days ago. ${account.timezone}.`}>
              <Input type="date" value={startDate} onChange={(e) => setStartDate(e.target.value)} />
            </Field>
            <Field label="To" hint="Default: today.">
              <Input type="date" value={endDate} onChange={(e) => setEndDate(e.target.value)} />
            </Field>
          </div>
        ) : null}
        {kind === "campaign_recipients" ? (
          <Field label="Campaign" hint="Status, opens, clicks and unsubscribes for each recipient, with the orders and revenue credited to the campaign.">
            <Select value={campaignId} onChange={(e) => setCampaignId(e.target.value)}>
              <option value="">Pick a sent campaign</option>
              {sent.map((c) => (
                <option key={c.id} value={c.id}>
                  {c.name}
                </option>
              ))}
            </Select>
          </Field>
        ) : null}
        {kind === "suppressions" ? <p className="text-sm text-zinc-600">Everyone who unsubscribed from email or texts, or whose email bounced hard or was marked as spam.</p> : null}
        <div className="flex flex-wrap items-center gap-3">
          <Button
            variant="primary"
            icon={<FileDown className="size-4" />}
            disabled={!req || busy}
            loading={start.isPending}
            onClick={() => req && start.mutate(req, { onSuccess: () => toast.success("Export started") })}
          >
            Start export
          </Button>
          <span className="text-xs text-zinc-500">
            {busy ? "Another export is running. Start the next one when it's done." : "Files are kept 7 days and download only while you're signed in."}
          </span>
        </div>
      </div>
    </Card>
  );
}

export function ExportsPage() {
  const exports = useExports();
  const { account } = useSession();
  const [downloading, setDownloading] = useState<string | null>(null);
  const files = exports.data ?? [];
  const busy = files.some((e) => e.kind !== "report" && (e.status === "queued" || e.status === "running"));
  return (
    <Page>
      <PageHeader title="Exports" description="Download your profiles, events, campaign results and unsubscribes as CSV files." />
      <div className="space-y-4">
        <NewExport busy={busy} />
        {exports.error ? <p className="text-sm text-red-600">{errorMessage(exports.error)}</p> : null}
        <Table>
          <thead>
            <tr>
              <Th>File</Th>
              <Th>Status</Th>
              <Th className="text-right">Rows</Th>
              <Th className="text-right">Size</Th>
              <Th>Started</Th>
              <Th />
            </tr>
          </thead>
          <tbody>
            {exports.isPending ? (
              <SkeletonRows cols={6} rows={3} />
            ) : files.length === 0 ? (
              <EmptyRow cols={6}>
                <EmptyState icon={<FileDown className="size-5" />} title="No exports yet" description="Files you export, and scheduled report emails, show up here for 7 days." />
              </EmptyRow>
            ) : (
              files.map((f) => (
                <tr key={f.id} className="[&:last-child>td]:border-b-0">
                  <Td>
                    <div className="font-medium text-zinc-900">{f.filename}</div>
                    <div className="text-xs text-zinc-500">{KIND_LABEL[f.kind]}</div>
                    {f.error ? <div className="mt-0.5 text-xs text-red-600">{f.error}</div> : null}
                  </Td>
                  <Td>
                    <StatusBadge status={f.status === "done" ? "verified" : f.status === "running" || f.status === "queued" ? "pending" : f.status === "failed" ? "failed" : "cancelled"} label={f.status === "done" ? "Ready" : f.status === "queued" ? "Waiting" : f.status === "running" ? "Writing" : f.status === "failed" ? "Failed" : "Expired"} />
                  </Td>
                  <Td className="text-right tabular-nums">{f.kind === "report" ? "—" : formatNumber(f.row_count)}</Td>
                  <Td className="text-right tabular-nums">{formatBytes(f.size_bytes)}</Td>
                  <Td className="text-zinc-500">{formatDateTime(f.created_at, account.timezone)}</Td>
                  <Td className="text-right">
                    {f.download_url ? (
                      <Button
                        size="sm"
                        icon={<Download className="size-3.5" />}
                        loading={downloading === f.id}
                        onClick={async () => {
                          setDownloading(f.id);
                          try {
                            await downloadCsv(f.download_url as string, undefined, f.filename);
                          } catch (e) {
                            toast.error(errorMessage(e));
                          } finally {
                            setDownloading(null);
                          }
                        }}
                      >
                        Download
                      </Button>
                    ) : null}
                  </Td>
                </tr>
              ))
            )}
          </tbody>
        </Table>
      </div>
    </Page>
  );
}
