import { useState, type FormEvent } from "react";
import { AlertTriangle, Plus, RotateCcw, Send, Trash2, Webhook } from "lucide-react";
import { errorMessage } from "../../api/client";
import { useSession } from "../../api";
import {
  Badge,
  Button,
  Card,
  Checkbox,
  CodeBlock,
  ConfirmDialog,
  CopyField,
  Drawer,
  EmptyRow,
  EmptyState,
  Field,
  Input,
  Modal,
  Notice,
  Segmented,
  SkeletonRows,
  Table,
  TagInput,
  Td,
  Th,
  Toggle,
  type Tone,
} from "../../components/ui";
import { formatDateTime, formatNumber, formatRelative } from "../../lib/format";
import { toast } from "../../lib/toast";
import {
  useCreateEndpoint,
  useDeleteEndpoint,
  useDeliveries,
  useDelivery,
  useEndpoints,
  useFlowSecret,
  useResend,
  useRotateFlowSecret,
  useRotateSecret,
  useSendTest,
  useTopics,
  useUpdateEndpoint,
} from "./api";
import type { DeliveryStatus, Endpoint, EndpointInput, EndpointStatus, TopicInfo } from "./types";

const DOCS = "https://github.com/tstout729/dumbmail/blob/main/docs/WEBHOOKS.md";

const statusTone: Record<EndpointStatus, Tone> = { active: "green", failing: "amber", disabled: "gray" };
const statusLabel: Record<EndpointStatus, string> = { active: "Active", failing: "Failing", disabled: "Off" };
const sourceLabel: Record<Endpoint["source"], string | null> = { dashboard: null, api: "API", zapier: "Zapier", make: "Make" };
const deliveryTone: Record<DeliveryStatus, Tone> = { delivered: "green", pending: "amber", sending: "amber", failed: "red", dropped: "red", cancelled: "gray" };
const deliveryLabel: Record<DeliveryStatus, string> = {
  delivered: "Delivered",
  pending: "Waiting",
  sending: "Sending",
  failed: "Failed",
  dropped: "Dropped",
  cancelled: "Cancelled",
};

/** What an error class in the delivery log means, in plain words. */
const errorText: Record<string, string> = {
  address_rejected: "Not a public https:// address",
  timeout: "No answer within 10 seconds",
  connect_failed: "Couldn't connect",
  request_failed: "The request failed",
  redirect: "Redirected (use the final URL)",
  http_error: "Error response",
  expired: "Not delivered within 24 hours",
  backlog_full: "Dropped: too many waiting",
  url_changed: "Cancelled: the URL changed",
  internal: "DumbMail couldn't send it",
};

function deliveryError(d: { error: string | null; response_code: number | null }): string {
  const text = d.error ? (errorText[d.error] ?? d.error) : "";
  if (d.response_code) return text ? `HTTP ${d.response_code} · ${text}` : `HTTP ${d.response_code}`;
  return text;
}

export function WebhooksPage() {
  return (
    <div className="space-y-4">
      <p className="text-sm text-zinc-500">
        Send what happens in DumbMail to your own systems as it happens: events, profile and consent changes, list and segment changes, message outcomes and more. Every request is signed so
        you can check it came from DumbMail.{" "}
        <a href={DOCS} target="_blank" rel="noreferrer" className="font-medium text-accent-700 hover:underline">
          Webhook reference
        </a>
      </p>
      <EndpointsCard />
      <FlowSecretCard />
      <VerifyCard />
    </div>
  );
}

function EndpointsCard() {
  const { account } = useSession();
  const endpoints = useEndpoints();
  const [adding, setAdding] = useState(false);
  const [secret, setSecret] = useState<{ title: string; secret: string } | null>(null);
  const [openId, setOpenId] = useState<string | null>(null);
  const open = (endpoints.data ?? []).find((e) => e.id === openId) ?? null;
  const rows = endpoints.data ?? [];

  return (
    <Card
      title="Endpoints"
      padded={false}
      actions={
        <Button size="sm" variant="primary" icon={<Plus className="size-3.5" />} onClick={() => setAdding(true)}>
          Add endpoint
        </Button>
      }
    >
      <Table className="rounded-none border-0 shadow-none">
        <thead>
          <tr>
            <Th className="w-[55%]">URL</Th>
            <Th className="hidden sm:table-cell">Topics</Th>
            <Th className="hidden sm:table-cell">Status</Th>
            <Th className="hidden text-right md:table-cell">Last 24 hours</Th>
          </tr>
        </thead>
        <tbody>
          {endpoints.isPending ? (
            <SkeletonRows cols={4} rows={2} />
          ) : endpoints.isError ? (
            <EmptyRow cols={4}>
              <p className="py-4 text-sm text-red-700">{errorMessage(endpoints.error)}</p>
            </EmptyRow>
          ) : rows.length === 0 ? (
            <EmptyRow cols={4}>
              <EmptyState
                icon={<Webhook className="size-5" />}
                title="No webhook endpoints"
                description="Add a URL and pick what to send it. Zapier and Make hooks show up here too."
                action={
                  <Button size="sm" variant="primary" icon={<Plus className="size-3.5" />} onClick={() => setAdding(true)}>
                    Add endpoint
                  </Button>
                }
              />
            </EmptyRow>
          ) : (
            rows.map((e) => (
              <tr key={e.id} className="cursor-pointer hover:bg-zinc-50" onClick={() => setOpenId(e.id)}>
                <Td className="max-w-0">
                  <button type="button" className="block w-full truncate text-left font-medium text-zinc-900 hover:underline" onClick={() => setOpenId(e.id)}>
                    {e.url}
                  </button>
                  <div className="flex items-center gap-1.5 truncate text-xs text-zinc-500">
                    <span className="sm:hidden">
                      <Badge tone={statusTone[e.status]} dot>
                        {statusLabel[e.status]}
                      </Badge>
                    </span>
                    {sourceLabel[e.source] ? <Badge tone="violet">{sourceLabel[e.source]}</Badge> : null}
                    <span className="truncate">{e.description || `Added ${formatDateTime(e.created_at, account.timezone)}`}</span>
                  </div>
                </Td>
                <Td className="hidden text-zinc-600 sm:table-cell">{e.topics.length === 1 ? e.topics[0] : `${e.topics.length} topics`}</Td>
                <Td className="hidden sm:table-cell">
                  <Badge tone={statusTone[e.status]} dot>
                    {statusLabel[e.status]}
                  </Badge>
                </Td>
                <Td className="hidden text-right text-zinc-600 tabular-nums md:table-cell">
                  {formatNumber(e.stats.delivered_24h)} sent
                  {e.stats.failed_24h > 0 ? <span className="text-red-700"> · {formatNumber(e.stats.failed_24h)} failed</span> : null}
                  {e.stats.pending > 0 ? <span className="text-amber-700"> · {formatNumber(e.stats.pending)} waiting</span> : null}
                </Td>
              </tr>
            ))
          )}
        </tbody>
      </Table>
      {adding ? (
        <EndpointForm
          onClose={() => setAdding(false)}
          onCreated={(s) => {
            setAdding(false);
            setSecret({ title: "Endpoint added", secret: s });
          }}
        />
      ) : null}
      {open ? <EndpointDrawer endpoint={open} onClose={() => setOpenId(null)} onSecret={(s) => setSecret({ title: "New signing secret", secret: s })} /> : null}
      {secret ? <SecretModal title={secret.title} secret={secret.secret} onClose={() => setSecret(null)} /> : null}
    </Card>
  );
}

function SecretModal({ title, secret, onClose }: { title: string; secret: string; onClose: () => void }) {
  return (
    <Modal
      title={title}
      onClose={onClose}
      size="md"
      footer={
        <Button variant="primary" onClick={onClose}>
          I've saved it
        </Button>
      }
    >
      <div className="space-y-3">
        <p className="text-sm text-zinc-600">Use this secret to check the signature on each request (see the code below the endpoints).</p>
        <div className="flex items-start gap-2 rounded-md bg-amber-50 px-3 py-2 text-sm text-amber-800">
          <AlertTriangle className="mt-0.5 size-4 shrink-0" />
          This is the only time you'll see this secret. Store it with the system that receives the webhooks.
        </div>
        <CopyField value={secret} />
      </div>
    </Modal>
  );
}

function groupTopics(topics: TopicInfo[]): [string, TopicInfo[]][] {
  const groups = new Map<string, TopicInfo[]>();
  for (const t of topics) groups.set(t.group, [...(groups.get(t.group) ?? []), t]);
  return [...groups.entries()];
}

function EndpointForm({ endpoint, onClose, onCreated }: { endpoint?: Endpoint; onClose: () => void; onCreated?: (secret: string) => void }) {
  const topics = useTopics();
  const create = useCreateEndpoint();
  const update = useUpdateEndpoint();
  const [url, setUrl] = useState(endpoint?.url ?? "https://");
  const [description, setDescription] = useState(endpoint?.description ?? "");
  const [selected, setSelected] = useState<string[]>(endpoint?.topics ?? []);
  const [metrics, setMetrics] = useState<string[]>(endpoint?.metrics ?? []);
  const [batch, setBatch] = useState(String(endpoint?.batch_size ?? 50));
  const [rate, setRate] = useState(String(endpoint?.rate_per_second ?? 10));
  const [trustedOnly, setTrustedOnly] = useState(endpoint?.trusted_only ?? false);
  const [error, setError] = useState<string | null>(null);
  const pending = create.isPending || update.isPending;
  const all = topics.data ?? [];

  const toggle = (t: string, on: boolean) => setSelected((s) => (on ? [...s, t] : s.filter((x) => x !== t)));

  const onSubmit = (e: FormEvent) => {
    e.preventDefault();
    setError(null);
    if (selected.length === 0) {
      setError("Pick at least one topic.");
      return;
    }
    const input: EndpointInput = {
      url: url.trim(),
      description: description.trim(),
      topics: selected,
      metrics: selected.includes("event.tracked") ? metrics : [],
      trusted_only: trustedOnly,
      batch_size: Number(batch),
      rate_per_second: Number(rate),
    };
    const onError = (err: unknown) => setError(errorMessage(err));
    if (endpoint) {
      update.mutate(
        { id: endpoint.id, input },
        {
          onSuccess: () => {
            toast.success("Endpoint saved");
            onClose();
          },
          onError,
        },
      );
    } else {
      create.mutate(input, { onSuccess: (created) => onCreated?.(created.secret), onError });
    }
  };

  return (
    <Modal title={endpoint ? "Edit endpoint" : "Add endpoint"} onClose={onClose} size="lg">
      <form onSubmit={onSubmit} className="space-y-4">
        <Field
          label="URL"
          hint={
            endpoint
              ? "Changing it cancels anything still waiting to go to the old URL, so nothing meant for one receiver goes to another."
              : "An https:// address on the public internet that accepts POST requests."
          }
        >
          <Input value={url} onChange={(e) => setUrl(e.target.value)} required autoFocus type="url" placeholder="https://example.com/dumbmail-webhook" />
        </Field>
        <Field label="Description" hint="Optional. So you know what this is for.">
          <Input value={description} onChange={(e) => setDescription(e.target.value)} maxLength={500} placeholder="Data team's ingest service" />
        </Field>
        <div>
          <div className="mb-1.5 flex items-center justify-between">
            <span className="text-sm font-medium text-zinc-800">Topics</span>
            <button type="button" className="text-xs font-medium text-accent-700 hover:underline" onClick={() => setSelected(selected.length === all.length ? [] : all.map((t) => t.topic))}>
              {selected.length === all.length && all.length > 0 ? "Clear all" : "Select all"}
            </button>
          </div>
          {topics.isPending ? (
            <p className="text-sm text-zinc-500">Loading topics…</p>
          ) : (
            <div className="grid gap-4 rounded-lg border border-zinc-200 p-3 sm:grid-cols-2">
              {groupTopics(all).map(([group, items]) => (
                <div key={group} className="space-y-1.5">
                  <div className="text-xs font-semibold uppercase tracking-wider text-zinc-400">{group}</div>
                  {items.map((t) => (
                    <Checkbox
                      key={t.topic}
                      checked={selected.includes(t.topic)}
                      onChange={(e) => toggle(t.topic, e.target.checked)}
                      label={<code className="text-xs">{t.topic}</code>}
                      description={t.description}
                    />
                  ))}
                </div>
              ))}
            </div>
          )}
        </div>
        {selected.includes("event.tracked") ? (
          <Field label="Only these metrics" hint="For event.tracked. Leave empty to send every event.">
            <TagInput value={metrics} onChange={setMetrics} placeholder="Placed Order" aria-label="Metrics" />
          </Field>
        ) : null}
        <Checkbox
          checked={trustedOnly}
          onChange={(e) => setTrustedOnly(e.target.checked)}
          label="Leave out events sent from a browser"
          description="Anyone with your site's public key can send events from a browser, with any values. Every event says who sent it (its source); turn this on if this endpoint acts on orders or money."
        />
        <div className="grid gap-4 sm:grid-cols-2">
          <Field label="Events per request" hint="1 sends one JSON object per request; more sends a JSON array.">
            <Input type="number" min={1} max={100} value={batch} onChange={(e) => setBatch(e.target.value)} required />
          </Field>
          <Field label="Requests per second, at most" hint="Keeps a busy account from flooding your server.">
            <Input type="number" min={1} max={100} value={rate} onChange={(e) => setRate(e.target.value)} required />
          </Field>
        </div>
        {error ? <p className="text-sm text-red-700">{error}</p> : null}
        <div className="flex justify-end gap-2">
          <Button variant="ghost" onClick={onClose}>
            Cancel
          </Button>
          <Button type="submit" variant="primary" loading={pending}>
            {endpoint ? "Save" : "Add endpoint"}
          </Button>
        </div>
      </form>
    </Modal>
  );
}

function EndpointDrawer({ endpoint, onClose, onSecret }: { endpoint: Endpoint; onClose: () => void; onSecret: (secret: string) => void }) {
  const { account } = useSession();
  const update = useUpdateEndpoint();
  const remove = useDeleteEndpoint();
  const rotate = useRotateSecret();
  const test = useSendTest();
  const [editing, setEditing] = useState(false);
  const [deleting, setDeleting] = useState(false);
  const [rotating, setRotating] = useState(false);

  const onTest = () =>
    test.mutate(
      { id: endpoint.id },
      {
        onSuccess: (r) => {
          if (r.ok) toast.success(`Test event delivered (HTTP ${r.delivery.response_code ?? ""})`);
          else toast.error(`Test event failed: ${deliveryError(r.delivery) || "no response"}`);
        },
      },
    );

  return (
    <Drawer title="Webhook endpoint" onClose={onClose} width="w-[min(640px,100vw)]">
      <div className="space-y-5">
        <div className="space-y-1">
          <p className="break-all font-medium text-zinc-900">{endpoint.url}</p>
          {endpoint.description ? <p className="text-sm text-zinc-500">{endpoint.description}</p> : null}
        </div>
        {endpoint.status === "disabled" && endpoint.disabled_reason ? (
          <Notice tone="amber" title="This endpoint is off">
            {endpoint.disabled_reason} Turn it back on once it's fixed, then resend what failed.
          </Notice>
        ) : endpoint.status === "failing" ? (
          <Notice tone="amber" title="Deliveries are failing">
            Failing since {formatDateTime(endpoint.failing_since, account.timezone)}.{" "}
            {endpoint.next_retry_at ? `Next try at ${formatDateTime(endpoint.next_retry_at, account.timezone)}. ` : ""}Deliveries are retried for 24 hours; an endpoint that fails for 3 days is turned off.
          </Notice>
        ) : null}
        {endpoint.stats.dropped_total > 0 ? (
          <Notice tone="amber" title="Some events were dropped">
            {formatNumber(endpoint.stats.dropped_total)} {endpoint.stats.dropped_total === 1 ? "event was" : "events were"} dropped because more than 100,000 were waiting
            {endpoint.stats.last_dropped_at ? ` (last ${formatRelative(endpoint.stats.last_dropped_at)})` : ""}. You can resend dropped events from the log for 14 days.
          </Notice>
        ) : null}
        <div className="flex flex-wrap items-center gap-2">
          <Toggle checked={endpoint.enabled} onChange={(on) => update.mutate({ id: endpoint.id, input: { enabled: on } })} label={endpoint.enabled ? "On" : "Off"} />
          <span className="mx-1 h-5 w-px bg-zinc-200" aria-hidden />
          <Button size="sm" icon={<Send className="size-3.5" />} onClick={onTest} loading={test.isPending}>
            Send test event
          </Button>
          <Button size="sm" onClick={() => setEditing(true)}>
            Edit
          </Button>
          <Button size="sm" icon={<RotateCcw className="size-3.5" />} onClick={() => setRotating(true)}>
            Rotate secret
          </Button>
          <Button size="sm" variant="danger" icon={<Trash2 className="size-3.5" />} onClick={() => setDeleting(true)}>
            Delete
          </Button>
        </div>
        <dl className="grid grid-cols-2 gap-x-4 gap-y-2 text-sm">
          <dt className="text-zinc-500">Topics</dt>
          <dd className="text-zinc-800">
            {endpoint.topics.join(", ")}
            {endpoint.metrics.length > 0 ? <span className="text-zinc-500"> ({endpoint.metrics.join(", ")})</span> : null}
          </dd>
          <dt className="text-zinc-500">Signing secret</dt>
          <dd className="font-mono text-xs text-zinc-700">
            {endpoint.secret_hint}
            {endpoint.previous_secret_valid_until ? <span className="font-sans text-zinc-500"> (old one works until {formatDateTime(endpoint.previous_secret_valid_until, account.timezone)})</span> : null}
          </dd>
          <dt className="text-zinc-500">Sources</dt>
          <dd className="text-zinc-800">{endpoint.trusted_only ? "Not events sent from a browser" : "All, each marked with its source"}</dd>
          <dt className="text-zinc-500">Per request</dt>
          <dd className="text-zinc-800">
            {endpoint.batch_size === 1 ? "1 event" : `up to ${endpoint.batch_size} events`}, at most {endpoint.rate_per_second} per second
          </dd>
          {endpoint.filter && endpoint.filter.length > 0 ? (
            <>
              <dt className="text-zinc-500">Filter</dt>
              <dd className="text-zinc-800">{endpoint.filter.map((f) => `${f.field} ${f.op} ${JSON.stringify(f.value ?? "")}`).join(" and ")}</dd>
            </>
          ) : null}
          <dt className="text-zinc-500">Last delivered</dt>
          <dd className="text-zinc-800">{formatRelative(endpoint.last_success_at)}</dd>
        </dl>
        <DeliveryLog endpoint={endpoint} />
      </div>
      {editing ? <EndpointForm endpoint={endpoint} onClose={() => setEditing(false)} /> : null}
      {deleting ? (
        <ConfirmDialog
          title="Delete this endpoint?"
          body="Nothing more is sent to it, and its delivery log is deleted."
          confirmLabel="Delete endpoint"
          danger
          loading={remove.isPending}
          onConfirm={() =>
            remove.mutate(endpoint.id, {
              onSuccess: () => {
                setDeleting(false);
                onClose();
              },
            })
          }
          onClose={() => setDeleting(false)}
        />
      ) : null}
      {rotating ? (
        <ConfirmDialog
          title="Rotate the signing secret?"
          body="You'll get a new secret. For the next 24 hours requests are signed with both, so you can update your receiver without missing anything."
          confirmLabel="Rotate secret"
          loading={rotate.isPending}
          onConfirm={() =>
            rotate.mutate(endpoint.id, {
              onSuccess: (r) => {
                setRotating(false);
                onSecret(r.secret);
              },
            })
          }
          onClose={() => setRotating(false)}
        />
      ) : null}
    </Drawer>
  );
}

function DeliveryLog({ endpoint }: { endpoint: Endpoint }) {
  const { account } = useSession();
  const [status, setStatus] = useState<DeliveryStatus | "">("");
  const [openId, setOpenId] = useState<string | null>(null);
  const deliveries = useDeliveries(endpoint.id, status);
  const resend = useResend();
  const rows = deliveries.data?.data ?? [];

  const resendFailed = () =>
    resend.mutate(
      { id: endpoint.id, since: new Date(Date.now() - 24 * 3600_000).toISOString(), status: "failed" },
      { onSuccess: (r) => toast.success(r.resent === 0 ? "Nothing failed in the last 24 hours" : `Resending ${formatNumber(r.resent)}`) },
    );

  return (
    <div>
      <div className="mb-2 flex flex-wrap items-center justify-between gap-2">
        <h3 className="text-sm font-semibold text-zinc-900">Delivery log</h3>
        <div className="flex items-center gap-2">
          <Segmented
            size="sm"
            value={status}
            onChange={setStatus}
            options={[
              { value: "", label: "All" },
              { value: "failed", label: "Failed" },
              { value: "pending", label: "Waiting" },
              { value: "delivered", label: "Delivered" },
              { value: "dropped", label: "Dropped" },
              { value: "cancelled", label: "Cancelled" },
            ]}
          />
          <Button size="sm" onClick={resendFailed} loading={resend.isPending} disabled={!endpoint.enabled}>
            Resend failed
          </Button>
        </div>
      </div>
      <div className="divide-y divide-zinc-100 rounded-lg border border-zinc-200 bg-white">
        {deliveries.isPending ? (
          <p className="px-3 py-4 text-sm text-zinc-500">Loading…</p>
        ) : rows.length === 0 ? (
          <p className="px-3 py-6 text-center text-sm text-zinc-500">No deliveries {status ? "with this status " : ""}in the last 14 days.</p>
        ) : (
          rows.map((d) => (
            <button key={d.id} type="button" onClick={() => setOpenId(d.id)} className="flex w-full items-start justify-between gap-3 px-3 py-2.5 text-left hover:bg-zinc-50">
              <span className="min-w-0">
                <code className="text-xs text-zinc-800">{d.topic}</code>
                {d.test ? <span className="ml-1.5 text-xs text-zinc-400">test</span> : null}
                <span className="block text-xs text-zinc-500" title={formatDateTime(d.created_at, account.timezone)}>
                  {formatRelative(d.created_at)}
                  {d.attempts > 1 ? ` · ${d.attempts} tries` : ""}
                </span>
              </span>
              <span className="flex min-w-0 shrink-0 flex-col items-end gap-0.5">
                <Badge tone={deliveryTone[d.status]}>{deliveryLabel[d.status]}</Badge>
                <span className="max-w-48 truncate text-xs text-zinc-500">{deliveryError(d)}</span>
              </span>
            </button>
          ))
        )}
      </div>
      <p className="mt-2 text-xs text-zinc-500">Deliveries are kept for 14 days. Each event has the same id on every try, so receivers can skip ones they've already handled.</p>
      {openId ? <DeliveryModal endpointId={endpoint.id} deliveryId={openId} canResend={endpoint.enabled} onClose={() => setOpenId(null)} /> : null}
    </div>
  );
}

function DeliveryModal({ endpointId, deliveryId, canResend, onClose }: { endpointId: string; deliveryId: string; canResend: boolean; onClose: () => void }) {
  const { account } = useSession();
  const d = useDelivery(endpointId, deliveryId);
  const resend = useResend();
  const data = d.data;
  return (
    <Modal
      title="Delivery"
      onClose={onClose}
      size="lg"
      footer={
        data && ["failed", "delivered", "dropped"].includes(data.status) && !data.test ? (
          <Button
            variant="primary"
            disabled={!canResend}
            loading={resend.isPending}
            onClick={() => resend.mutate({ id: endpointId, delivery_ids: [deliveryId], status: "all" }, { onSuccess: () => { toast.success("Resending"); onClose(); } })}
          >
            Resend
          </Button>
        ) : undefined
      }
    >
      {d.isPending ? (
        <p className="text-sm text-zinc-500">Loading…</p>
      ) : d.isError || !data ? (
        <p className="text-sm text-red-700">{errorMessage(d.error)}</p>
      ) : (
        <div className="space-y-4">
          <dl className="grid grid-cols-2 gap-x-4 gap-y-1.5 text-sm">
            <dt className="text-zinc-500">Status</dt>
            <dd>
              <Badge tone={deliveryTone[data.status]}>{deliveryLabel[data.status]}</Badge>
            </dd>
            <dt className="text-zinc-500">Created</dt>
            <dd className="text-zinc-800">{formatDateTime(data.created_at, account.timezone)}</dd>
            <dt className="text-zinc-500">Tries</dt>
            <dd className="text-zinc-800">
              {data.attempts}
              {data.next_attempt_at ? ` · next ${formatRelative(data.next_attempt_at)}` : ""}
            </dd>
            <dt className="text-zinc-500">Response</dt>
            <dd className="text-zinc-800">
              {deliveryError(data) || "None yet"}
              {data.duration_ms !== null ? ` in ${formatNumber(data.duration_ms)} ms` : ""}
            </dd>
            <dt className="text-zinc-500">Event id</dt>
            <dd className="break-all font-mono text-xs text-zinc-700">{data.event_id}</dd>
          </dl>
          <CodeBlock code={JSON.stringify(data.payload, null, 2)} label="Payload" />
        </div>
      )}
    </Modal>
  );
}

function FlowSecretCard() {
  const { account } = useSession();
  const info = useFlowSecret();
  const rotate = useRotateFlowSecret();
  const [confirming, setConfirming] = useState(false);
  const [secret, setSecret] = useState<string | null>(null);
  const hint = info.data?.secret_hint ?? null;
  return (
    <Card title="Flow webhook steps">
      <p className="mb-3 text-sm text-zinc-600">
        Webhook steps in flows are signed the same way, with one secret for the whole account. The request body hasn't changed, so existing receivers keep working.
      </p>
      <div className="flex flex-wrap items-center justify-between gap-3">
        <div className="text-sm">
          {info.isPending ? (
            <span className="text-zinc-500">Loading…</span>
          ) : hint ? (
            <>
              <span className="text-zinc-500">Secret </span>
              <code className="font-mono text-xs text-zinc-700">{hint}</code>
              <span className="text-zinc-500"> · made {formatDateTime(info.data?.created_at, account.timezone)}</span>
            </>
          ) : (
            <span className="text-zinc-500">No secret yet. One is made the first time a flow webhook runs; make one now to see it.</span>
          )}
        </div>
        <Button size="sm" icon={<RotateCcw className="size-3.5" />} onClick={() => (hint ? setConfirming(true) : rotate.mutate(undefined, { onSuccess: (r) => setSecret(r.secret) }))} loading={rotate.isPending}>
          {hint ? "New secret" : "Make secret"}
        </Button>
      </div>
      {confirming ? (
        <ConfirmDialog
          title="Make a new flow webhook secret?"
          body="The current secret stops working right away. Update the systems that receive your flow webhooks at the same time."
          confirmLabel="Make new secret"
          loading={rotate.isPending}
          onConfirm={() =>
            rotate.mutate(undefined, {
              onSuccess: (r) => {
                setConfirming(false);
                setSecret(r.secret);
              },
            })
          }
          onClose={() => setConfirming(false)}
        />
      ) : null}
      {secret ? <SecretModal title="Flow webhook secret" secret={secret} onClose={() => setSecret(null)} /> : null}
    </Card>
  );
}

const nodeSample = `import crypto from "node:crypto";

// rawBody: the request body as a Buffer, before JSON parsing.
function verifyDumbMail(rawBody, headers, secret) {
  const ts = headers["dumbmail-webhook-timestamp"];
  if (Math.abs(Date.now() / 1000 - Number(ts)) > 300) return false;
  const expected = crypto.createHmac("sha256", secret).update(\`\${ts}.\`).update(rawBody).digest("hex");
  return (headers["dumbmail-webhook-signature"] ?? "")
    .split(",")
    .map((s) => s.trim().replace(/^v1=/, ""))
    .some((s) => s.length === expected.length && crypto.timingSafeEqual(Buffer.from(s), Buffer.from(expected)));
}`;

function VerifyCard() {
  return (
    <Card title="Check the signature">
      <p className="mb-3 text-sm text-zinc-600">
        Each request has <code className="rounded bg-zinc-100 px-1 text-xs">DumbMail-Webhook-Timestamp</code> and <code className="rounded bg-zinc-100 px-1 text-xs">DumbMail-Webhook-Signature</code>{" "}
        headers. The signature is an HMAC-SHA256 of the timestamp, a dot and the raw body, made with the endpoint's secret. Requests are retried for 24 hours, and the order isn't guaranteed: use
        each event's <code className="rounded bg-zinc-100 px-1 text-xs">id</code> to skip duplicates.
      </p>
      <CodeBlock code={nodeSample} label="Node.js" />
      <p className="mt-2 text-xs text-zinc-500">
        Python, Ruby, PHP and Rust versions are in the{" "}
        <a href={DOCS} target="_blank" rel="noreferrer" className="font-medium text-accent-700 hover:underline">
          webhook reference
        </a>
        .
      </p>
    </Card>
  );
}
