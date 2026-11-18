import { useDeferredValue, useState, type FormEvent } from "react";
import { Link } from "react-router";
import { AlertTriangle, Code2, Plus, Receipt, Search } from "lucide-react";
import { useSession } from "../../api";
import {
  Badge,
  Button,
  Card,
  CodeBlock,
  Drawer,
  EmptyRow,
  EmptyState,
  Field,
  Input,
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
  Tabs,
  Td,
  Textarea,
  Th,
  type Tone,
} from "../../components/ui";
import { formatDateTime, formatNumber, formatRelative } from "../../lib/format";
import { toast } from "../../lib/toast";
import { pageSize, useCreateTransactionalTemplate, useTransactionalSend, useTransactionalSends, useTransactionalSettings, useTransactionalTemplates } from "./api";
import type { SendStatus } from "./types";

const statusTone: Record<SendStatus, Tone> = {
  queued: "blue",
  sending: "blue",
  sent: "gray",
  delivered: "green",
  opened: "green",
  clicked: "green",
  bounced: "red",
  complained: "red",
  skipped: "amber",
  failed: "red",
  unknown: "amber",
};

const statuses: SendStatus[] = ["queued", "sending", "sent", "delivered", "opened", "clicked", "bounced", "complained", "skipped", "failed", "unknown"];

function StatusPill({ status }: { status: SendStatus }) {
  return (
    <Badge tone={statusTone[status]} dot>
      {status.charAt(0).toUpperCase() + status.slice(1)}
    </Badge>
  );
}

type Tab = "sends" | "templates";

export function TransactionalPage() {
  const [tab, setTab] = useState<Tab>("sends");
  const [howTo, setHowTo] = useState(false);
  return (
    <Page wide>
      <PageHeader
        title="Transactional email"
        description="Receipts, password resets and other email people trigger, sent one at a time through the API."
        actions={
          <Button icon={<Code2 className="size-3.5" />} onClick={() => setHowTo(true)}>
            How to send
          </Button>
        }
      />
      <Limits />
      <Tabs
        value={tab}
        onChange={setTab}
        tabs={[
          { value: "sends", label: "Sent email" },
          { value: "templates", label: "Templates" },
        ]}
      />
      {tab === "sends" ? <Sends /> : <Templates />}
      {howTo ? <HowTo onClose={() => setHowTo(false)} /> : null}
    </Page>
  );
}

function Limits() {
  const s = useTransactionalSettings();
  return (
    <div className="mb-4 space-y-3">
      {s.data?.mode === "off" ? (
        <Notice tone="amber" icon={<AlertTriangle />} title="Transactional email is off">
          Nothing can be sent through the API until the account owner turns it on in{" "}
          <Link to="/settings/transactional" className="font-medium underline">
            Settings → Transactional
          </Link>
          .
        </Notice>
      ) : s.data?.mode === "restricted" ? (
        <Notice tone="blue" title="Restricted mode">
          Only saved transactional templates can be sent, as saved (no subject change, variables up to 500 characters), up to {formatNumber(s.data.daily_limit)} a day.{" "}
          <Link to="/settings/transactional" className="font-medium underline">
            How to get full mode
          </Link>
        </Notice>
      ) : null}
      {s.data?.paused ? (
        <Notice tone="red" icon={<AlertTriangle />} title="Transactional email is paused for this account">
          {s.data.paused_reason ? `${s.data.paused_reason}. ` : null}Contact support to resume. Campaigns and flows aren't affected.
        </Notice>
      ) : null}
      <div className="grid grid-cols-2 gap-3 md:grid-cols-3">
        <Stat label="Last 24 hours" loading={s.isPending} value={formatNumber(s.data?.sent_last_24h)} sub={s.data ? `of ${formatNumber(s.data.daily_limit)} allowed` : undefined} />
        <Stat label="Last minute" loading={s.isPending} value={formatNumber(s.data?.sent_last_minute)} sub={s.data ? `of ${formatNumber(s.data.per_minute_limit)} allowed` : undefined} />
        <div className="col-span-2 rounded-lg border border-zinc-200 bg-white px-4 py-3 text-xs text-zinc-500 shadow-xs md:col-span-1">
          Transactional email skips marketing consent, so it can't promote anything. It never goes to addresses that bounced or marked email as spam, and it
          has no unsubscribe link.
        </div>
      </div>
    </div>
  );
}

function Sends() {
  const { account } = useSession();
  const [q, setQ] = useState("");
  const [status, setStatus] = useState<SendStatus | "">("");
  const [offset, setOffset] = useState(0);
  const [open, setOpen] = useState<string | null>(null);
  const search = useDeferredValue(q.trim());
  const sends = useTransactionalSends({ q: search, status, offset });
  const rows = sends.data ?? [];
  return (
    <>
      <div className="mb-3 flex flex-wrap items-center gap-2">
        <div className="relative min-w-0 flex-1 sm:max-w-xs">
          <Search className="pointer-events-none absolute left-2.5 top-1/2 size-3.5 -translate-y-1/2 text-zinc-400" />
          <Input
            value={q}
            onChange={(e) => {
              setQ(e.target.value);
              setOffset(0);
            }}
            placeholder="Search by recipient or subject"
            aria-label="Search sent email"
            className="pl-8"
          />
        </div>
        <Select
          value={status}
          onChange={(e) => {
            setStatus(statuses.find((s) => s === e.target.value) ?? "");
            setOffset(0);
          }}
          aria-label="Status"
          className="w-40"
        >
          <option value="">All statuses</option>
          {statuses.map((s) => (
            <option key={s} value={s}>
              {s.charAt(0).toUpperCase() + s.slice(1)}
            </option>
          ))}
        </Select>
      </div>
      {sends.error ? <p className="mb-3 text-sm text-red-600">{sends.error.message}</p> : null}
      <div className="overflow-x-auto">
        <Table>
          <thead>
            <tr>
              <Th>Recipient</Th>
              <Th>Subject</Th>
              <Th>Status</Th>
              <Th className="text-right">Sent</Th>
            </tr>
          </thead>
          <tbody>
            {sends.isPending ? (
              <SkeletonRows cols={4} />
            ) : rows.length === 0 ? (
              <EmptyRow cols={4}>
                {search || status ? (
                  "No email matches."
                ) : (
                  <EmptyState icon={<Receipt className="size-5" />} title="No transactional email yet" description="Email you send with POST /api/transactional/send shows up here." />
                )}
              </EmptyRow>
            ) : (
              rows.map((r) => (
                <tr key={r.id} className="cursor-pointer hover:bg-zinc-50" onClick={() => setOpen(r.id)}>
                  <Td className="max-w-[220px] truncate font-medium text-zinc-900">
                    <button type="button" className="text-left hover:underline" onClick={() => setOpen(r.id)}>
                      {r.to}
                    </button>
                  </Td>
                  <Td className="max-w-[320px] truncate text-zinc-600">{r.subject ?? "—"}</Td>
                  <Td>
                    <StatusPill status={r.status} />
                  </Td>
                  <Td className="whitespace-nowrap text-right text-zinc-500" title={formatDateTime(r.created_at, account.timezone)}>
                    {formatRelative(r.sent_at ?? r.created_at)}
                  </Td>
                </tr>
              ))
            )}
          </tbody>
        </Table>
      </div>
      {offset > 0 || rows.length === pageSize ? (
        <div className="mt-3 flex justify-end gap-2">
          <Button size="sm" disabled={offset === 0} onClick={() => setOffset(Math.max(0, offset - pageSize))}>
            Newer
          </Button>
          <Button size="sm" disabled={rows.length < pageSize} onClick={() => setOffset(offset + pageSize)}>
            Older
          </Button>
        </div>
      ) : null}
      {open ? <SendDrawer id={open} onClose={() => setOpen(null)} /> : null}
    </>
  );
}

function SendDrawer({ id, onClose }: { id: string; onClose: () => void }) {
  const { account } = useSession();
  const send = useTransactionalSend(id);
  const s = send.data;
  const tz = account.timezone;
  return (
    <Drawer title="Transactional email" onClose={onClose}>
      {send.isPending ? (
        <div className="space-y-3">
          <Skeleton className="h-6 w-2/3" />
          <Skeleton className="h-24" />
        </div>
      ) : send.error ? (
        <p className="text-sm text-red-600">{send.error.message}</p>
      ) : s ? (
        <div className="space-y-4 text-sm">
          <div>
            <div className="font-medium text-zinc-900">{s.subject ?? "No subject"}</div>
            <div className="mt-0.5 flex flex-wrap items-center gap-2 text-zinc-600">
              {s.profile_id ? (
                <Link to={`/audience/profiles/${s.profile_id}`} className="text-accent-700 hover:underline">
                  {s.to}
                </Link>
              ) : (
                s.to
              )}
              <StatusPill status={s.status} />
            </div>
            {s.error ? <p className="mt-2 rounded-md bg-amber-50 px-2.5 py-1.5 text-xs text-amber-900">{s.error}</p> : null}
          </div>
          <dl className="grid grid-cols-[7rem_minmax(0,1fr)] gap-x-3 gap-y-1.5 text-xs">
            {(
              [
                ["Requested", s.created_at],
                ["Sent", s.sent_at],
                ["Delivered", s.delivered_at],
                ["Opened", s.opened_at],
                ["Clicked", s.clicked_at],
              ] as const
            ).map(([label, at]) => (
              <div key={label} className="contents">
                <dt className="text-zinc-500">{label}</dt>
                <dd className="text-zinc-800">{at ? formatDateTime(at, tz) : "—"}</dd>
              </div>
            ))}
            <dt className="text-zinc-500">Attempts</dt>
            <dd className="text-zinc-800">{s.attempts}</dd>
            <dt className="text-zinc-500">Click tracking</dt>
            <dd className="text-zinc-800">{s.track_clicks ? "On" : "Off"}</dd>
            <dt className="text-zinc-500">Sent through</dt>
            <dd className="text-zinc-800">{s.via === "api" ? "API" : s.via === "mcp" ? "MCP" : s.via.charAt(0).toUpperCase() + s.via.slice(1)}</dd>
            {s.idempotency_key ? (
              <>
                <dt className="text-zinc-500">Idempotency key</dt>
                <dd className="break-all font-mono text-zinc-800">{s.idempotency_key}</dd>
              </>
            ) : null}
          </dl>
          {Object.keys(s.metadata).length ? <CodeBlock label="Metadata" code={JSON.stringify(s.metadata, null, 2)} /> : null}
          <p className="text-xs text-zinc-500">
            {s.status === "unknown"
              ? "The hand-off to the email provider ended without a clear answer, so it may have gone out. It isn't charged and is never sent again automatically. "
              : null}
            Variables and the email body aren't kept, since they can hold reset links and codes. To send it again, call the API again with fresh values.
          </p>
        </div>
      ) : null}
    </Drawer>
  );
}

function Templates() {
  const { account } = useSession();
  const templates = useTransactionalTemplates();
  const [creating, setCreating] = useState(false);
  const rows = templates.data ?? [];
  return (
    <>
      <div className="mb-3 flex items-center justify-between gap-3">
        <p className="text-sm text-zinc-500">
          Only templates made here can be sent through the transactional API. Each one is checked for promotional wording when it's saved and every time it's
          sent.
        </p>
        <Button variant="primary" icon={<Plus className="size-4" />} onClick={() => setCreating(true)}>
          New template
        </Button>
      </div>
      {templates.error ? <p className="mb-3 text-sm text-red-600">{templates.error.message}</p> : null}
      <div className="overflow-x-auto">
        <Table>
          <thead>
            <tr>
              <Th>Name</Th>
              <Th>Subject</Th>
              <Th>Template id</Th>
              <Th className="text-right">Updated</Th>
            </tr>
          </thead>
          <tbody>
            {templates.isPending ? (
              <SkeletonRows cols={4} rows={3} />
            ) : rows.length === 0 ? (
              <EmptyRow cols={4}>
                <EmptyState title="No transactional templates" description="Make one for each email your store sends: receipt, shipping notice, password reset." />
              </EmptyRow>
            ) : (
              rows.map((t) => (
                <tr key={t.id}>
                  <Td className="font-medium">
                    <Link to={`/content/templates/${t.id}`} className="text-zinc-900 hover:underline">
                      {t.name}
                    </Link>
                    {t.review.blocked ? (
                      <span className="ml-2" title={`Reads like marketing, so it won't send: ${t.review.flags.join(", ")}`}>
                        <Badge tone="red">Won't send</Badge>
                      </span>
                    ) : null}
                  </Td>
                  <Td className="max-w-[280px] truncate text-zinc-600">{t.content.subject ?? "—"}</Td>
                  <Td className="font-mono text-xs text-zinc-500">{t.id}</Td>
                  <Td className="whitespace-nowrap text-right text-zinc-500" title={formatDateTime(t.updated_at, account.timezone)}>
                    {formatRelative(t.updated_at)}
                  </Td>
                </tr>
              ))
            )}
          </tbody>
        </Table>
      </div>
      {creating ? <NewTemplate onClose={() => setCreating(false)} /> : null}
    </>
  );
}

function NewTemplate({ onClose }: { onClose: () => void }) {
  const create = useCreateTransactionalTemplate();
  const [format, setFormat] = useState<"text" | "html">("text");
  const onSubmit = (e: FormEvent<HTMLFormElement>) => {
    e.preventDefault();
    const d = new FormData(e.currentTarget);
    const body = String(d.get("body") ?? "");
    const subject = String(d.get("subject") ?? "").trim();
    create.mutate(
      { name: String(d.get("name") ?? "").trim(), content: format === "text" ? { subject, text: body } : { subject, html: body } },
      {
        onSuccess: (t) => {
          toast.success(`Template "${t.name}" saved`);
          onClose();
        },
      },
    );
  };
  return (
    <Modal title="New transactional template" description="Use {{ variable }} tags for values you send with each email, like {{ order_id }}." onClose={onClose} size="lg">
      <form onSubmit={onSubmit} className="space-y-4">
        <Field label="Name">
          <Input name="name" required maxLength={200} placeholder="Order receipt" autoFocus />
        </Field>
        <Field label="Subject">
          <Input name="subject" required maxLength={998} placeholder="Order {{ order_id }} confirmed" />
        </Field>
        <Field label="Body" hint={format === "text" ? "Plain text. Blank lines start new paragraphs." : "HTML inside the email's frame, or a full document."}>
          <div className="space-y-2">
            <Segmented
              value={format}
              onChange={setFormat}
              options={[
                { value: "text", label: "Text" },
                { value: "html", label: "HTML" },
              ]}
            />
            <Textarea name="body" required rows={8} className="font-mono text-xs" placeholder={"Thanks for your order, {{ first_name|default('there') }}.\n\nTotal: {{ total }}"} />
          </div>
        </Field>
        <p className="text-xs text-zinc-500">For a designed layout, save it here and then edit it with blocks in Templates.</p>
        {create.error ? <p className="text-sm text-red-600">{create.error.message}</p> : null}
        <div className="flex justify-end gap-2">
          <Button type="button" variant="ghost" onClick={onClose}>
            Cancel
          </Button>
          <Button type="submit" variant="primary" loading={create.isPending}>
            Save template
          </Button>
        </div>
      </form>
    </Modal>
  );
}

function HowTo({ onClose }: { onClose: () => void }) {
  const origin = window.location.origin;
  const settings = useTransactionalSettings();
  const curl = `curl ${origin}/api/transactional/send \\
  -H "Authorization: Bearer <key>" \\
  -H "Content-Type: application/json" \\
  -d '{
    "to": { "email": "ann@example.com", "name": "Ann Lee" },
    "template_id": "<template id>",
    "variables": { "order_id": "1042", "total": "$36.00" },
    "idempotency_key": "receipt-1042"
  }'`;
  return (
    <Drawer title="How to send" onClose={onClose} width="w-[min(560px,100vw)]">
      <div className="space-y-4 text-sm text-zinc-600">
        <p>
          Send one email per request with an API key from{" "}
          <Link to="/settings/developers" className="text-accent-700 hover:underline">
            Developers
          </Link>
          . Use a template id from the Templates tab
          {settings.data?.mode === "full" ? (
            <>
              , or send <code className="rounded bg-zinc-100 px-1 text-xs">content</code> with a subject and text or HTML
            </>
          ) : null}
          .
        </p>
        <CodeBlock label="Terminal" code={curl} />
        <ul className="list-disc space-y-1 pl-5">
          <li>Retry with the same idempotency key as often as you like: it sends once.</li>
          <li>Variables aren't stored. To send again, make a new request with fresh values (a new reset link, for example).</li>
          <li>Click tracking is off unless you send "track_clicks": true. Opens are tracked.</li>
          <li>Add "address_footer": true to show your name and postal address.</li>
          <li>Addresses that bounced or complained come back with status "skipped".</li>
        </ul>
        <Card title="Limits" className="shadow-none">
          <p className="text-xs">
            {settings.data ? `${formatNumber(settings.data.per_minute_limit)} emails a minute and ${formatNumber(settings.data.daily_limit)} in any 24 hours. ` : null}
            Promotional wording is refused (discount or offer language, "shop now", links to several products), and sudden floods to people with no
            other activity pause transactional email until support takes a look.
          </p>
        </Card>
      </div>
    </Drawer>
  );
}
