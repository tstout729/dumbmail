import { useState, type ChangeEvent } from "react";
import { Link, useNavigate, useParams } from "react-router";
import { ChevronLeft, ChevronRight, Download, Plus, Search, Trash2 } from "lucide-react";
import { useSession } from "../../api";
import {
  Badge,
  Button,
  Card,
  CodeBlock,
  ConfirmDialog,
  EmptyRow,
  EmptyState,
  Field,
  IconButton,
  Input,
  InlineError,
  Menu,
  Modal,
  Notice,
  Page,
  PageHeader,
  Segmented,
  Select,
  Skeleton,
  SkeletonRows,
  Stat,
  Table,
  Td,
  Textarea,
  Th,
  buttonClass,
} from "../../components/ui";
import { formatCurrency, formatDate, formatNumber, formatPercent } from "../../lib/format";
import { useDebouncedValue } from "../../lib/hooks";
import { toast } from "../../lib/toast";
import { useCan } from "../team/api";
import { exportUrl, useAddCodes, useCoupon, useCouponCodes, useCouponReport, useDeleteCoupon, useGenerateCodes, useUpdateCoupon } from "./api";
import { CouponSettings, draftFrom, draftProblem, expiryText, inputFrom, StockBadge } from "./parts";
import type { AddCodesResult, Coupon, CouponCode, CouponCodeStatus } from "./types";

export function CouponPage() {
  const { id = "" } = useParams();
  const coupon = useCoupon(id);
  if (coupon.isPending) {
    return (
      <Page>
        <Skeleton className="mb-5 h-8 w-64" />
        <Skeleton className="h-64 w-full" />
      </Page>
    );
  }
  if (coupon.error) {
    return (
      <Page>
        <EmptyState title="Couldn't load this coupon" description={coupon.error.message} action={<Link to="/content/coupons" className={buttonClass("secondary")}>Back to coupons</Link>} />
      </Page>
    );
  }
  return <CouponView c={coupon.data} />;
}

function CouponView({ c }: { c: Coupon }) {
  const { account } = useSession();
  const navigate = useNavigate();
  const report = useCouponReport(c.id);
  const remove = useDeleteCoupon();
  const [adding, setAdding] = useState(false);
  const [deleting, setDeleting] = useState(false);
  // Codes are money and the holder list is people's data: managers and up, with profile access.
  const canManage = useCan("catalog.write");
  const canReadProfiles = useCan("profiles.read");
  const canSeeCodes = canManage && canReadProfiles;
  const canDelete = useCan("catalog.delete");
  const r = report.data;

  return (
    <Page>
      <PageHeader
        breadcrumbs={[{ label: "Coupons", to: "/content/coupons" }]}
        title={
          <span className="flex flex-wrap items-center gap-2.5">
            <span className="font-mono">{c.name}</span>
            <StockBadge c={c} />
          </span>
        }
        description={[c.description, expiryText(c, account.timezone)].filter(Boolean).join(" · ")}
        actions={
          <>
            {canSeeCodes ? (
            <Menu
              trigger={(p) => (
                <button type="button" className={buttonClass("secondary")} {...p}>
                  <Download className="size-3.5" />
                  Export
                </button>
              )}
              items={[
                { label: "Codes not given out yet, for your store (.txt)", onSelect: () => window.location.assign(exportUrl(c.id, "available", "codes")) },
                { label: "Every code, one per line (.txt)", onSelect: () => window.location.assign(exportUrl(c.id, "all", "codes")) },
                { label: "Every code, with who has it, for spreadsheets (.csv)", onSelect: () => window.location.assign(exportUrl(c.id, "all", "csv")) },
              ]}
            />
            ) : null}
            {canDelete ? (
              <IconButton label="Delete coupon" onClick={() => setDeleting(true)}>
                <Trash2 className="size-4" />
              </IconButton>
            ) : null}
            {canManage ? (
              <Button variant="primary" icon={<Plus className="size-3.5" />} onClick={() => setAdding(true)}>
                Add codes
              </Button>
            ) : null}
          </>
        }
      />
      <Alerts c={c} />
      <div className="mb-5 grid grid-cols-2 gap-3 md:grid-cols-4">
        <Stat label="Available" value={formatNumber(c.stock.available)} sub={c.stock.generating ? `making ${formatNumber(c.stock.generating)} more…` : `of ${formatNumber(c.stock.total)} codes`} />
        <Stat label="Given out" value={formatNumber(r?.given)} loading={report.isPending} sub="reached someone" />
        <Stat
          label="Redeemed"
          value={formatNumber(r?.redeemed)}
          loading={report.isPending}
          sub={r ? `${formatPercent(r.redemption_rate)} of given${r.redeemed_unverified ? ` · ${formatNumber(r.redeemed_unverified)} unverified` : ""}` : undefined}
        />
        <Stat label="Revenue" value={formatCurrency(r?.revenue)} loading={report.isPending} sub="from verified orders" />
      </div>
      <div className="grid grid-cols-1 gap-5 lg:grid-cols-5">
        <div className="space-y-5 lg:col-span-3">
          {canSeeCodes ? (
            <CodesCard c={c} timeZone={account.timezone} />
          ) : (
            <Card title="Codes">
              <p className="text-sm text-zinc-600">Only people who manage coupons and can see profiles see the codes and who has them.</p>
            </Card>
          )}
        </div>
        <div className="space-y-5 lg:col-span-2">
          <UsageCard c={c} />
          {canManage ? <SettingsCard c={c} /> : null}
        </div>
      </div>
      {r?.note ? <p className="mt-4 text-xs text-zinc-500">{r.note}</p> : null}
      {adding ? <AddCodesDialog c={c} onClose={() => setAdding(false)} /> : null}
      {deleting ? (
        <ConfirmDialog
          title={`Delete ${c.name}?`}
          body={`This deletes all ${formatNumber(c.stock.total)} codes, who got which code, and the report. It can't be undone. A coupon that a segment, flow, campaign or form uses can't be deleted until you change them.`}
          confirmLabel="Delete coupon"
          danger
          loading={remove.isPending}
          onConfirm={() =>
            remove.mutate(c.id, {
              onSuccess: () => {
                toast.success("Coupon deleted");
                navigate("/content/coupons");
              },
            })
          }
          onClose={() => setDeleting(false)}
        />
      ) : null}
    </Page>
  );
}

function Alerts({ c }: { c: Coupon }) {
  if (c.alerts.out_of_codes) {
    return (
      <div className="mb-4">
        <Notice tone="red" title="Out of codes">
          Messages that use this coupon are waiting, and sign-up forms show no code. Add codes and they carry on within 10 minutes.
          {c.alerts.waiting.length ? (
            <ul className="mt-1.5 list-disc space-y-0.5 pl-5">
              {c.alerts.waiting.map((w) => (
                <li key={`${w.type}-${w.id}`}>
                  <Link className="underline" to={w.type === "campaign" ? `/campaigns/${w.id}` : `/flows/${w.id}`}>
                    {w.name ?? (w.type === "campaign" ? "A campaign" : "A flow")}
                  </Link>{" "}
                  ({w.type === "campaign" ? "campaign, paused" : "flow, waiting"})
                </li>
              ))}
            </ul>
          ) : null}
        </Notice>
      </div>
    );
  }
  if (c.alerts.low_stock) {
    return (
      <div className="mb-4">
        <Notice tone="amber" title="Running low">
          {formatNumber(c.stock.available)} codes left, below your alert level of {formatNumber(c.low_stock_threshold)}.
        </Notice>
      </div>
    );
  }
  if (c.codes_total === 0) {
    return (
      <div className="mb-4">
        <Notice tone="blue" title="Add codes to start">
          {c.source === "generated" ? "Generate codes, export them and import them into your store." : "Paste or upload the single-use codes you made in your store."} Until then, messages that use this coupon wait.
        </Notice>
      </div>
    );
  }
  return null;
}

function UsageCard({ c }: { c: Coupon }) {
  return (
    <Card title="Use it">
      <div className="space-y-3 text-sm text-zinc-700">
        <p>Put this in the body of an email or a text. Each person sees their own code{c.new_code_each_time ? ", a new one in every message" : ", the same one in every message"}.</p>
        <CodeBlock code={c.usage.tag} />
        <p>Its expiry date:</p>
        <CodeBlock code={c.usage.expiry_tag} />
        <p>
          In the email builder, add a <strong>Coupon</strong> block. On a sign-up form, pick this coupon on the thank-you screen.
        </p>
        <p className="text-xs text-zinc-500">Codes can't go in a subject or preheader. Previews and test sends show SAMPLE-CODE and use no code. A code given to someone stays theirs.</p>
      </div>
    </Card>
  );
}

function SettingsCard({ c }: { c: Coupon }) {
  const { account } = useSession();
  const update = useUpdateCoupon(c.id);
  const [draft, setDraft] = useState(() => draftFrom(c, account.timezone));
  const [tried, setTried] = useState(false);
  const problem = draftProblem(draft, false);
  const save = () => {
    setTried(true);
    if (problem) return;
    update.mutate(inputFrom(draft, account.timezone, false), { onSuccess: () => toast.success("Coupon saved") });
  };
  return (
    <Card
      title="Settings"
      actions={
        <Button size="sm" variant="primary" loading={update.isPending} onClick={save}>
          Save
        </Button>
      }
    >
      <CouponSettings draft={draft} onChange={setDraft} creating={false} />
      <div className="mt-3">
        <InlineError message={tried ? problem : null} />
      </div>
    </Card>
  );
}

const pageSize = 25;

function CodesCard({ c, timeZone }: { c: Coupon; timeZone: string }) {
  const [search, setSearch] = useState("");
  const [status, setStatus] = useState<CouponCodeStatus | "">("");
  const q = useDebouncedValue(search.trim(), 250);
  const filterKey = `${q}|${status}`;
  // Paging belongs to one search; a new search starts at the first page. `cursors` holds where each
  // earlier page started, so Previous can go back.
  const [paging, setPaging] = useState<{ key: string; cursors: (number | undefined)[] }>({ key: "", cursors: [] });
  const cursors = paging.key === filterKey ? paging.cursors : [];
  const cursor = cursors.length ? cursors[cursors.length - 1] : undefined;
  const codes = useCouponCodes(c.id, { q: q || undefined, status, limit: pageSize, cursor });
  const rows = codes.data?.data ?? [];
  const total = codes.data?.total ?? 0;
  const next = codes.data?.next_cursor ?? null;
  const offset = cursors.length * pageSize;
  return (
    <Card title="Codes" padded={false}>
      <div className="flex flex-wrap items-center gap-2 border-b border-zinc-100 p-3">
        <div className="relative min-w-0 flex-1">
          <Search className="pointer-events-none absolute left-2.5 top-2.5 size-3.5 text-zinc-400" />
          <Input value={search} onChange={(e) => setSearch(e.target.value)} placeholder="Find a code, or a person's email" aria-label="Search codes" className="pl-8" />
        </div>
        <Select value={status} onChange={(e) => setStatus(e.target.value as CouponCodeStatus | "")} className="w-auto" aria-label="Status">
          <option value="">Any status</option>
          <option value="available">Available</option>
          <option value="assigned">Given out</option>
          <option value="redeemed">Redeemed</option>
        </Select>
      </div>
      <Table className="rounded-none border-0">
        <thead>
          <tr>
            <Th>Code</Th>
            <Th>Status</Th>
            <Th className="hidden sm:table-cell">Person</Th>
            <Th className="hidden md:table-cell">Given</Th>
          </tr>
        </thead>
        <tbody>
          {codes.isPending ? (
            <SkeletonRows cols={4} rows={6} />
          ) : codes.error ? (
            <EmptyRow cols={4}>
              <EmptyState title="Couldn't load codes" description={codes.error.message} />
            </EmptyRow>
          ) : rows.length === 0 ? (
            <EmptyRow cols={4}>
              <EmptyState title={q || status ? "No matches" : "No codes yet"} description={q || status ? "No codes match this search." : "Add codes to start giving them out."} />
            </EmptyRow>
          ) : (
            rows.map((code) => <CodeRow key={code.id} code={code} timeZone={timeZone} />)
          )}
        </tbody>
      </Table>
      {total > pageSize ? (
        <div className="flex items-center justify-end gap-2 border-t border-zinc-100 px-3 py-2 text-xs text-zinc-500">
          <span className="tabular-nums">
            {formatNumber(offset + 1)}–{formatNumber(Math.min(offset + pageSize, total))} of {formatNumber(total)}
            {codes.data?.total_capped ? "+" : ""}
          </span>
          <IconButton label="Previous page" disabled={cursors.length === 0} onClick={() => setPaging({ key: filterKey, cursors: cursors.slice(0, -1) })}>
            <ChevronLeft className="size-4" />
          </IconButton>
          <IconButton label="Next page" disabled={next === null} onClick={() => next !== null && setPaging({ key: filterKey, cursors: [...cursors, next] })}>
            <ChevronRight className="size-4" />
          </IconButton>
        </div>
      ) : null}
    </Card>
  );
}

function CodeRow({ code, timeZone }: { code: CouponCode; timeZone: string }) {
  const badge =
    code.status === "redeemed" ? (
      <Badge tone="green">Redeemed</Badge>
    ) : code.status === "assigned" ? (
      code.unverified_order_at ? (
        <Badge tone="amber">Used (unverified)</Badge>
      ) : code.delivered_at ? (
        <Badge tone="blue">Given out</Badge>
      ) : (
        <Badge tone="gray">Kept for them</Badge>
      )
    ) : (
      <Badge tone="gray">Available</Badge>
    );
  return (
    <tr>
      <Td className="max-w-[9rem] truncate font-mono text-zinc-900 sm:max-w-none">{code.code}</Td>
      <Td>{badge}</Td>
      <Td className="hidden max-w-48 truncate sm:table-cell">
        {code.profile_id ? (
          <Link to={`/audience/profiles/${code.profile_id}`} className="text-zinc-700 hover:underline">
            {code.email ?? "A profile"}
          </Link>
        ) : (
          <span className="text-zinc-400">—</span>
        )}
      </Td>
      <Td className="hidden whitespace-nowrap text-zinc-600 md:table-cell">
        {code.given_at ? formatDate(code.given_at, timeZone) : "—"}
        {code.order_value !== null ? <span className="text-zinc-400"> · {formatCurrency(code.order_value)}</span> : null}
      </Td>
    </tr>
  );
}

const maxFileBytes = 16 * 1024 * 1024;

function addedText(r: AddCodesResult): string {
  const parts = [`${formatNumber(r.added)} codes added`];
  if (r.duplicates) parts.push(`${formatNumber(r.duplicates)} already here`);
  if (r.refused) {
    const lines = r.refused_lines.map((l) => `line ${formatNumber(l.line)} (${l.reason})`).join(", ");
    parts.push(`${formatNumber(r.refused)} lines skipped: ${lines}${r.refused > r.refused_lines.length ? ", …" : ""}`);
  }
  return `${parts.join("; ")}.${r.stopped ? ` ${r.stopped}` : ""}`;
}

function AddCodesDialog({ c, onClose }: { c: Coupon; onClose: () => void }) {
  const [mode, setMode] = useState<"upload" | "generate">(c.source === "generated" ? "generate" : "upload");
  const [text, setText] = useState("");
  const [file, setFile] = useState<File | null>(null);
  const [count, setCount] = useState("1000");
  const add = useAddCodes(c.id);
  const generate = useGenerateCodes(c.id);
  const [done, setDone] = useState<string | null>(null);
  const n = /^\d+$/.test(count) ? Number(count) : NaN;
  const countError = mode === "generate" && !(n >= 1 && n <= 200_000) ? "Generate 1 to 200,000 codes at a time." : null;
  const onFile = (e: ChangeEvent<HTMLInputElement>) => {
    const f = e.target.files?.[0];
    if (!f) {
      setFile(null);
      return;
    }
    if (f.size > maxFileBytes) {
      toast.error("That file is larger than 16 MB. Split it into smaller files.");
      e.target.value = "";
      return;
    }
    setFile(f);
  };
  const submit = () => {
    if (mode === "upload") {
      add.mutate(file ?? text, { onSuccess: (r) => setDone(addedText(r)) });
    } else if (!countError) {
      generate.mutate(
        { count: n },
        {
          onSuccess: (r) =>
            setDone(
              r.queued
                ? `Making ${formatNumber(r.queued)} codes in the background; this page shows the progress. When they're done, export them and import them into your store before you send.`
                : `${formatNumber(r.added)} codes made. Export them and import them into your store before you send.`,
            ),
        },
      );
    }
  };
  return (
    <Modal
      title={`Add codes to ${c.name}`}
      onClose={onClose}
      size="lg"
      footer={
        done ? (
          <Button variant="primary" onClick={onClose}>
            Done
          </Button>
        ) : (
          <>
            <Button variant="ghost" onClick={onClose}>
              Cancel
            </Button>
            <Button
              variant="primary"
              loading={add.isPending || generate.isPending}
              disabled={mode === "upload" ? !file && !text.trim() : Boolean(countError) || !c.pattern}
              onClick={submit}
            >
              {mode === "upload" ? "Add codes" : "Generate"}
            </Button>
          </>
        )
      }
    >
      {done ? (
        <p className="text-sm text-zinc-800">{done}</p>
      ) : (
        <div className="space-y-4">
          <Segmented
            value={mode}
            onChange={setMode}
            options={[
              { value: "upload", label: "Paste or upload" },
              { value: "generate", label: "Generate" },
            ]}
          />
          {mode === "upload" ? (
            <>
              <Field label="CSV or text file" hint={file ? `${file.name} will be added. Clear it to paste codes instead.` : "Up to 16 MB and 200,000 codes."}>
                <input type="file" accept=".csv,.txt,text/csv,text/plain" onChange={onFile} className="block w-full text-sm text-zinc-700 file:mr-3 file:rounded-md file:border-0 file:bg-zinc-100 file:px-3 file:py-1.5 file:text-sm file:font-medium" />
              </Field>
              <Field label="Or paste codes" hint="One per line (the first column of a CSV): 4 to 64 letters, digits, - or _. Codes already in this coupon are skipped; letter case doesn't matter.">
                <Textarea rows={8} value={text} disabled={Boolean(file)} onChange={(e) => setText(e.target.value)} placeholder={"WELCOME-7KQ2\nWELCOME-9XPM\n…"} className="font-mono text-xs" />
              </Field>
            </>
          ) : c.pattern ? (
            <>
              <Field label="How many" error={countError}>
                <Input value={count} inputMode="numeric" onChange={(e) => setCount(e.target.value)} className="w-40" />
              </Field>
              <p className="text-sm text-zinc-600">
                Codes look like <span className="font-mono">{`${c.pattern.prefix ?? ""}${"X".repeat(c.pattern.length)}`}</span>. Change the pattern in Settings.
              </p>
            </>
          ) : (
            <InlineError message="Switch this coupon to “DumbMail makes them” in Settings and set a pattern first." />
          )}
        </div>
      )}
    </Modal>
  );
}
