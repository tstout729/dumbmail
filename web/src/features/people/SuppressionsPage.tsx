import { useState, type ChangeEvent } from "react";
import { Link } from "react-router";
import { ChevronLeft, ChevronRight, Download, Info, Plus, Search, ShieldOff, Upload } from "lucide-react";
import { errorMessage, useSession } from "../../api";
import {
  Badge,
  Button,
  ConfirmDialog,
  EmptyRow,
  EmptyState,
  Field,
  IconButton,
  InlineError,
  Input,
  Modal,
  Notice,
  Page,
  PageHeader,
  Select,
  SkeletonRows,
  Table,
  Tabs,
  Td,
  Textarea,
  Th,
  buttonClass,
  type Tone,
} from "../../components/ui";
import { formatDateTime, formatNumber, formatPhone, fullName, pluralize } from "../../lib/format";
import { useDebouncedValue } from "../../lib/hooks";
import { toast } from "../../lib/toast";
import { suppressionsExportUrl, useSuppress, useSuppressions, useUnsuppress } from "./api";
import { BulkProgress } from "./BulkActions";
import type { Channel, Suppression, SuppressionReason, SuppressResult } from "./types";

const limit = 50;

const reasonTone: Record<SuppressionReason, Tone> = {
  unsubscribed: "gray",
  hard_bounce: "red",
  complaint: "red",
  manual: "amber",
  pending: "blue",
  not_subscribed: "gray",
};

const reasonOptions: { value: SuppressionReason; label: string }[] = [
  { value: "unsubscribed", label: "Unsubscribed" },
  { value: "hard_bounce", label: "Hard bounce" },
  { value: "complaint", label: "Spam complaint" },
  { value: "manual", label: "Suppressed by hand" },
  { value: "pending", label: "Hasn't confirmed sign-up" },
  { value: "not_subscribed", label: "Never agreed to texts" },
];

export function SuppressionsPage() {
  const { account } = useSession();
  const tz = account.timezone;
  const [channel, setChannel] = useState<"all" | Channel>("all");
  const [reason, setReason] = useState<SuppressionReason | "">("");
  const [search, setSearch] = useState("");
  const q = useDebouncedValue(search.trim(), 250);
  const [from, setFrom] = useState("");
  const [to, setTo] = useState("");
  const filters = { q: q || undefined, channel: channel === "all" ? undefined : channel, reason: reason || undefined, from: from || undefined, to: to || undefined };
  const filterKey = JSON.stringify(filters);
  const [paging, setPaging] = useState({ key: filterKey, offset: 0 });
  const offset = paging.key === filterKey ? paging.offset : 0;
  const list = useSuppressions({ ...filters, limit, offset });
  const [adding, setAdding] = useState(false);
  const [lifting, setLifting] = useState<Suppression | null>(null);
  const [whyNot, setWhyNot] = useState<Suppression | null>(null);
  const unsuppress = useUnsuppress();
  const rows = list.data?.data;
  const total = list.data?.total ?? 0;
  const count = (c: Channel) => (list.data?.counts ?? []).filter((x) => x.channel === c).reduce((n, x) => n + x.count, 0);

  return (
    <Page wide>
      <PageHeader
        title="Suppression list"
        description="Everyone who can't get your marketing emails or texts, and why."
        actions={
          <>
            <a href={suppressionsExportUrl(filters)} download className={buttonClass("secondary", "md")}>
              <Download className="size-3.5" /> Export CSV
            </a>
            <Button variant="primary" icon={<Plus className="size-3.5" />} onClick={() => setAdding(true)}>
              Suppress addresses
            </Button>
          </>
        }
      />
      <div className="mb-4">
        <Notice tone="gray" icon={<Info />} title="Only suppressions added by hand can be lifted.">
          Bounced addresses, spam complaints and people who unsubscribed themselves stay suppressed. Only the person can subscribe again, with one of your sign-up forms or by texting START.
          {list.data && list.data.deleted_profiles_suppressed > 0
            ? ` ${pluralize(list.data.deleted_profiles_suppressed, "deleted profile")} also stay suppressed through a one-way code of their address, in case they're imported again.`
            : ""}
        </Notice>
      </div>
      <Tabs
        value={channel}
        onChange={setChannel}
        tabs={[
          { value: "all", label: "All" },
          { value: "email", label: "Email", count: list.data ? count("email") : undefined },
          { value: "sms", label: "Text messages", count: list.data ? count("sms") : undefined },
        ]}
      />
      <div className="mb-3 flex flex-wrap items-end gap-2">
        <div className="relative w-full sm:w-64">
          <Search className="pointer-events-none absolute left-2.5 top-2.5 size-3.5 text-zinc-400" />
          <Input value={search} onChange={(e) => setSearch(e.target.value)} placeholder="Search address or name" className="pl-8" aria-label="Search" />
        </div>
        <Select value={reason} onChange={(e) => setReason(e.target.value as SuppressionReason | "")} className="w-full sm:w-52" aria-label="Reason">
          <option value="">Any reason</option>
          {reasonOptions.map((r) => (
            <option key={r.value} value={r.value}>
              {r.label}
            </option>
          ))}
        </Select>
        <label className="flex items-center gap-1.5 text-xs text-zinc-500">
          From
          <Input type="date" value={from} onChange={(e) => setFrom(e.target.value)} className="w-36" aria-label="From date" />
        </label>
        <label className="flex items-center gap-1.5 text-xs text-zinc-500">
          To
          <Input type="date" value={to} onChange={(e) => setTo(e.target.value)} className="w-36" aria-label="To date" />
        </label>
      </div>
      {list.error ? <InlineError message={errorMessage(list.error)} /> : null}
      <Table>
        <thead>
          <tr>
            <Th>Address</Th>
            <Th>Channel</Th>
            <Th>Reason</Th>
            <Th>Since</Th>
            <Th className="w-28" />
          </tr>
        </thead>
        <tbody>
          {list.isPending && !rows ? (
            <SkeletonRows cols={5} rows={8} />
          ) : (rows ?? []).length === 0 ? (
            <EmptyRow cols={5}>
              <EmptyState
                icon={<ShieldOff className="size-5" />}
                title={q || reason || from || to ? "No matches" : "Nobody is suppressed"}
                description={q || reason || from || to ? "Try other filters." : "People who unsubscribe, bounce or report spam show up here."}
              />
            </EmptyRow>
          ) : (
            (rows ?? []).map((r) => (
              <tr key={`${r.profile_id}-${r.channel}`} className="hover:bg-zinc-50/60">
                <Td>
                  <Link to={`/audience/profiles/${r.profile_id}`} className="whitespace-nowrap font-medium text-zinc-900 hover:underline">
                    {r.channel === "sms" ? formatPhone(r.address) : r.address}
                  </Link>
                  {fullName(r) ? <div className="text-xs text-zinc-500">{fullName(r)}</div> : null}
                </Td>
                <Td className="text-zinc-600">{r.channel === "sms" ? "Texts" : "Email"}</Td>
                <Td>
                  <Badge tone={reasonTone[r.reason]}>{r.reason_text}</Badge>
                  {r.how ? <div className="mt-0.5 text-xs text-zinc-500">{r.how}</div> : null}
                </Td>
                <Td className="whitespace-nowrap text-zinc-600">{r.since ? formatDateTime(r.since, tz) : "Before history started"}</Td>
                <Td className="text-right">
                  {r.can_unsuppress ? (
                    <Button size="sm" onClick={() => setLifting(r)}>
                      Lift
                    </Button>
                  ) : (
                    <Button size="sm" variant="ghost" onClick={() => setWhyNot(r)}>
                      Why not?
                    </Button>
                  )}
                </Td>
              </tr>
            ))
          )}
        </tbody>
      </Table>
      {total > limit ? (
        <div className="mt-3 flex items-center justify-end gap-2 text-xs text-zinc-500">
          <span className="tabular-nums">
            {formatNumber(offset + 1)}–{formatNumber(Math.min(offset + limit, total))} of {formatNumber(total)}
          </span>
          <IconButton label="Previous page" disabled={offset === 0} onClick={() => setPaging({ key: filterKey, offset: Math.max(0, offset - limit) })}>
            <ChevronLeft className="size-4" />
          </IconButton>
          <IconButton label="Next page" disabled={offset + limit >= total} onClick={() => setPaging({ key: filterKey, offset: offset + limit })}>
            <ChevronRight className="size-4" />
          </IconButton>
        </div>
      ) : null}
      {adding ? <SuppressDialog onClose={() => setAdding(false)} /> : null}
      {lifting ? (
        <ConfirmDialog
          title="Lift this suppression?"
          body={`${lifting.address} can get your ${lifting.channel === "sms" ? "texts" : "marketing email"} again if they're subscribed. Their consent stays as it is; this doesn't subscribe them.`}
          confirmLabel="Lift suppression"
          loading={unsuppress.isPending}
          onConfirm={() =>
            unsuppress.mutate(
              { profile_id: lifting.profile_id, channel: lifting.channel },
              {
                onSuccess: () => {
                  toast.success("Suppression lifted");
                  setLifting(null);
                },
              },
            )
          }
          onClose={() => setLifting(null)}
        />
      ) : null}
      {whyNot ? (
        <Modal title="This can't be lifted" onClose={() => setWhyNot(null)} size="sm" footer={<Button onClick={() => setWhyNot(null)}>OK</Button>}>
          <p className="text-sm text-zinc-600">{whyNot.why_not}</p>
        </Modal>
      ) : null}
    </Page>
  );
}

function SuppressDialog({ onClose }: { onClose: () => void }) {
  const [text, setText] = useState("");
  const [csv, setCsv] = useState<{ name: string; text: string } | null>(null);
  const [preview, setPreview] = useState<SuppressResult | null>(null);
  const [jobId, setJobId] = useState<string | null>(null);
  const suppress = useSuppress();
  const input = { addresses: text.trim() ? [text] : [], csv: csv?.text };

  if (jobId) return <BulkProgress id={jobId} onClose={onClose} />;

  const onFile = (e: ChangeEvent<HTMLInputElement>) => {
    const f = e.target.files?.[0];
    if (!f) return;
    if (f.size > 20 * 1024 * 1024) {
      toast.error("That file is over 20 MB. Split it into smaller files.");
      return;
    }
    void f.text().then((t) => {
      setCsv({ name: f.name, text: t });
      setPreview(null);
    });
  };

  return (
    <Modal
      title="Suppress addresses"
      description="They'll get no marketing on that channel. New addresses are added as profiles."
      onClose={onClose}
      size="md"
      footer={
        <>
          <Button variant="ghost" onClick={onClose}>
            Cancel
          </Button>
          {preview?.dry_run ? (
            <Button
              variant="primary"
              loading={suppress.isPending}
              onClick={() =>
                suppress.mutate(
                  { ...input, confirm: true },
                  {
                    onSuccess: (r) => {
                      if (r.bulk_action) {
                        setJobId(r.bulk_action.id);
                        return;
                      }
                      toast.success(`Suppressed ${pluralize(r.suppressed ?? 0, "address", "addresses")}${r.already_suppressed ? ` (${formatNumber(r.already_suppressed)} already were)` : ""}`);
                      onClose();
                    },
                  },
                )
              }
            >
              Suppress {pluralize(preview.count ?? 0, "address", "addresses")}
            </Button>
          ) : (
            <Button variant="primary" disabled={!text.trim() && !csv} loading={suppress.isPending} onClick={() => suppress.mutate({ ...input, confirm: false }, { onSuccess: setPreview })}>
              Check addresses
            </Button>
          )}
        </>
      }
    >
      <div className="space-y-3">
        <Field label="Emails or phone numbers" hint="One per line, or separated by commas.">
          <Textarea
            value={text}
            onChange={(e) => {
              setText(e.target.value);
              setPreview(null);
            }}
            rows={5}
            placeholder={"jane@example.com\n+15035550100"}
            autoFocus
          />
        </Field>
        <div className="flex flex-wrap items-center gap-2 text-sm">
          <label className={buttonClass("secondary", "sm", "cursor-pointer")}>
            <Upload className="size-3.5" /> Upload CSV
            <input type="file" accept=".csv,text/csv,text/plain" className="sr-only" onChange={onFile} />
          </label>
          {csv ? (
            <span className="text-zinc-600">
              {csv.name}{" "}
              <button type="button" className="text-xs text-zinc-500 underline" onClick={() => setCsv(null)}>
                remove
              </button>
            </span>
          ) : (
            <span className="text-xs text-zinc-500">A column named email or phone is used, or else the first column.</span>
          )}
        </div>
        {preview?.dry_run ? (
          <div className="rounded-lg bg-zinc-50 px-3 py-2 text-sm text-zinc-700">
            {pluralize(preview.count ?? 0, "address", "addresses")} will be suppressed.
            {preview.invalid_count > 0 ? (
              <span className="block text-xs text-amber-800">
                Skipping {pluralize(preview.invalid_count, "entry", "entries")} that aren't an email or phone number: {preview.invalid.slice(0, 5).join(", ")}
                {preview.invalid_count > 5 ? "…" : ""}
              </span>
            ) : null}
          </div>
        ) : null}
        {suppress.error ? <InlineError message={errorMessage(suppress.error)} /> : null}
      </div>
    </Modal>
  );
}
