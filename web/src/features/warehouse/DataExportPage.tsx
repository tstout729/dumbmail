import { useState, type FormEvent } from "react";
import { Database, Play, Plus, Trash2 } from "lucide-react";
import { errorMessage } from "../../api/client";
import { useSession } from "../../api";
import {
  Badge,
  Button,
  Card,
  Checkbox,
  ConfirmDialog,
  EmptyState,
  Field,
  Input,
  Modal,
  Notice,
  Segmented,
  Select,
  Toggle,
  type Tone,
} from "../../components/ui";
import { formatDateTime, formatNumber } from "../../lib/format";
import { toast } from "../../lib/toast";
import { useCreateDestination, useDatasets, useDeleteDestination, useDestinations, useRuns, useStartRun, useTestDestination, useUpdateDestination } from "./api";
import type { Destination, DestinationInput, Format, Run, RunStatus, Schedule } from "./types";

const DOCS = "https://github.com/tstout729/dumbmail/blob/main/docs/DATA_EXPORT.md";

type Provider = "aws" | "r2" | "gcs" | "b2" | "other";

const providers: Record<Provider, { label: string; endpoint: string; region: string; regionHint: string; keyHint: string }> = {
  aws: { label: "Amazon S3", endpoint: "", region: "us-east-1", regionHint: "The bucket's region.", keyHint: "An IAM user's access key with s3:PutObject on the bucket." },
  r2: { label: "Cloudflare R2", endpoint: "https://<account id>.r2.cloudflarestorage.com", region: "auto", regionHint: "auto for R2.", keyHint: "An R2 API token with Object Read & Write on this bucket." },
  gcs: { label: "Google Cloud Storage", endpoint: "https://storage.googleapis.com", region: "auto", regionHint: "auto for Cloud Storage.", keyHint: "An HMAC key (Cloud Storage → Settings → Interoperability)." },
  b2: { label: "Backblaze B2", endpoint: "https://s3.us-west-004.backblazeb2.com", region: "us-west-004", regionHint: "The region in your bucket's S3 endpoint.", keyHint: "An application key with writeFiles on the bucket." },
  other: { label: "Other S3-compatible", endpoint: "https://", region: "us-east-1", regionHint: "Your provider's region name.", keyHint: "An S3 access key that can write to the bucket." },
};

function providerOf(d: Destination): Provider {
  const e = d.endpoint ?? "";
  if (!e) return "aws";
  if (e.includes("r2.cloudflarestorage.com")) return "r2";
  if (e.includes("storage.googleapis.com")) return "gcs";
  if (e.includes("backblazeb2.com")) return "b2";
  return "other";
}

const runTone: Record<RunStatus, Tone> = { queued: "gray", running: "amber", succeeded: "green", failed: "red" };
const runLabel: Record<RunStatus, string> = { queued: "Queued", running: "Running", succeeded: "Done", failed: "Failed" };
const scheduleLabel: Record<Schedule, string> = { hourly: "Every hour", daily: "Daily at 1am", manual: "Only when started" };

function formatBytes(n: number): string {
  if (n < 1024) return `${n} B`;
  if (n < 1024 * 1024) return `${(n / 1024).toFixed(1)} KB`;
  if (n < 1024 ** 3) return `${(n / 1024 ** 2).toFixed(1)} MB`;
  return `${(n / 1024 ** 3).toFixed(2)} GB`;
}

export function DataExportPage() {
  const destinations = useDestinations();
  const [adding, setAdding] = useState(false);
  const rows = destinations.data ?? [];
  return (
    <div className="space-y-4">
      <p className="text-sm text-zinc-500">
        Copy your data to your own storage bucket on a schedule, as files Snowflake, BigQuery, Redshift and other warehouses can load. Each run adds only what changed since the last one.{" "}
        <a href={DOCS} target="_blank" rel="noreferrer" className="font-medium text-accent-700 hover:underline">
          File layout and warehouse SQL
        </a>
      </p>
      <div className="flex justify-end">
        <Button size="sm" variant="primary" icon={<Plus className="size-3.5" />} onClick={() => setAdding(true)}>
          Add destination
        </Button>
      </div>
      {destinations.isPending ? (
        <Card>
          <p className="text-sm text-zinc-500">Loading…</p>
        </Card>
      ) : destinations.isError ? (
        <Notice tone="red" title="Couldn't load destinations">
          {errorMessage(destinations.error)}
        </Notice>
      ) : rows.length === 0 ? (
        <Card>
          <EmptyState
            icon={<Database className="size-5" />}
            title="No export destinations"
            description="Add an Amazon S3, Cloudflare R2, Google Cloud Storage or Backblaze B2 bucket to start."
            action={
              <Button size="sm" variant="primary" icon={<Plus className="size-3.5" />} onClick={() => setAdding(true)}>
                Add destination
              </Button>
            }
          />
        </Card>
      ) : (
        rows.map((d) => <DestinationCard key={d.id} d={d} />)
      )}
      {adding ? <DestinationForm onClose={() => setAdding(false)} /> : null}
    </div>
  );
}

function DestinationCard({ d }: { d: Destination }) {
  const { account } = useSession();
  const start = useStartRun();
  const test = useTestDestination();
  const update = useUpdateDestination();
  const remove = useDeleteDestination();
  const [editing, setEditing] = useState(false);
  const [confirmFull, setConfirmFull] = useState(false);
  const [deleting, setDeleting] = useState(false);
  const running = d.last_run?.status === "queued" || d.last_run?.status === "running";

  const run = (kind: "incremental" | "full") =>
    start.mutate({ id: d.id, kind }, { onSuccess: () => toast.success(kind === "full" ? "Full export started" : "Export started"), onSettled: () => setConfirmFull(false) });

  return (
    <Card
      title={
        <span className="flex items-center gap-2">
          {d.name}
          {!d.enabled ? <Badge>Off</Badge> : null}
        </span>
      }
      actions={<Toggle checked={d.enabled} onChange={(on) => update.mutate({ id: d.id, input: { enabled: on } })} label={d.enabled ? "On" : "Off"} />}
    >
      <div className="space-y-4">
        <dl className="grid gap-x-4 gap-y-1.5 text-sm sm:grid-cols-[10rem_1fr]">
          <dt className="text-zinc-500">Bucket</dt>
          <dd className="break-all text-zinc-800">
            {providers[providerOf(d)].label} · <code className="text-xs">{d.bucket}/{d.prefix}</code>
          </dd>
          <dt className="text-zinc-500">Schedule</dt>
          <dd className="text-zinc-800">
            {scheduleLabel[d.schedule]}
            {d.next_run_at ? <span className="text-zinc-500"> · next {formatDateTime(d.next_run_at, account.timezone)}</span> : null}
          </dd>
          <dt className="text-zinc-500">Files</dt>
          <dd className="text-zinc-800">{d.format === "csv" ? "CSV" : "JSON lines"}, gzip'd · {d.datasets.length} datasets</dd>
          <dt className="text-zinc-500">Key</dt>
          <dd className="font-mono text-xs text-zinc-700">{d.access_key_hint}</dd>
        </dl>
        {d.last_run?.status === "failed" ? (
          <Notice tone="red" title="The last export failed">
            {d.last_run.error ?? "Unknown error"}. The next export starts from where the last good one ended, so nothing is skipped once it works.
          </Notice>
        ) : null}
        {d.last_test_error ? (
          <Notice tone="amber" title="The last test write failed">
            {d.last_test_error}
          </Notice>
        ) : null}
        <div className="flex flex-wrap gap-2">
          <Button size="sm" variant="primary" icon={<Play className="size-3.5" />} onClick={() => run("incremental")} loading={start.isPending && !confirmFull} disabled={running}>
            Export changes now
          </Button>
          <Button size="sm" onClick={() => setConfirmFull(true)} disabled={running}>
            Full export
          </Button>
          <Button
            size="sm"
            loading={test.isPending}
            onClick={() => test.mutate(d.id, { onSuccess: (r) => (r.ok ? toast.success("DumbMail can write to this bucket") : toast.error(r.error ?? "Test write failed")) })}
          >
            Test
          </Button>
          <Button size="sm" onClick={() => setEditing(true)}>
            Edit
          </Button>
          <Button size="sm" variant="danger" icon={<Trash2 className="size-3.5" />} onClick={() => setDeleting(true)}>
            Delete
          </Button>
        </div>
        <div>
          <h3 className="mb-2 text-sm font-semibold text-zinc-900">Recent runs</h3>
          <RunsTable id={d.id} />
        </div>
      </div>
      {editing ? <DestinationForm destination={d} onClose={() => setEditing(false)} /> : null}
      {confirmFull ? (
        <ConfirmDialog
          title="Start a full export?"
          body={`This writes every row of ${d.datasets.length} datasets to ${d.bucket}. Large accounts can take a while and add storage costs. Use it to fill a new warehouse table.`}
          confirmLabel="Start full export"
          loading={start.isPending}
          onConfirm={() => run("full")}
          onClose={() => setConfirmFull(false)}
        />
      ) : null}
      {deleting ? (
        <ConfirmDialog
          title={`Delete "${d.name}"?`}
          body="Exports to this bucket stop and its keys and run history are deleted. Files already in the bucket stay there."
          confirmLabel="Delete destination"
          danger
          loading={remove.isPending}
          onConfirm={() => remove.mutate(d.id, { onSettled: () => setDeleting(false) })}
          onClose={() => setDeleting(false)}
        />
      ) : null}
    </Card>
  );
}

function duration(r: Run): string {
  if (!r.started_at || !r.finished_at) return "";
  const s = Math.max(0, Math.round((Date.parse(r.finished_at) - Date.parse(r.started_at)) / 1000));
  return s < 90 ? `${s}s` : `${Math.round(s / 60)} min`;
}

function RunsTable({ id }: { id: string }) {
  const { account } = useSession();
  const runs = useRuns(id);
  const rows = runs.data ?? [];
  return (
    <div className="divide-y divide-zinc-100 rounded-lg border border-zinc-200 bg-white">
      {runs.isPending ? (
        <p className="px-3 py-4 text-sm text-zinc-500">Loading…</p>
      ) : rows.length === 0 ? (
        <p className="px-3 py-6 text-center text-sm text-zinc-500">No exports yet.</p>
      ) : (
        rows.map((r) => (
          <div key={r.id} className="flex items-start justify-between gap-3 px-3 py-2.5" title={r.datasets.map((x) => `${x.name}: ${formatNumber(x.rows)}`).join("\n")}>
            <div className="min-w-0 text-sm">
              <div className="text-zinc-800">
                {r.kind === "full" ? "Full export" : "Changes"} <span className="text-zinc-500">· {formatDateTime(r.created_at, account.timezone)}</span>
              </div>
              <div className="truncate text-xs text-zinc-500">
                {r.status === "failed" ? r.error : r.status === "running" && r.error ? `Retrying: ${r.error}` : `${formatNumber(r.files)} files · ${formatBytes(r.bytes)}${duration(r) ? ` · ${duration(r)}` : ""}`}
              </div>
            </div>
            <div className="flex shrink-0 flex-col items-end gap-0.5">
              <Badge tone={runTone[r.status]}>{runLabel[r.status]}</Badge>
              <span className="text-xs tabular-nums text-zinc-500">{formatNumber(r.rows)} rows</span>
            </div>
          </div>
        ))
      )}
    </div>
  );
}

function DestinationForm({ destination, onClose }: { destination?: Destination; onClose: () => void }) {
  const datasets = useDatasets();
  const create = useCreateDestination();
  const update = useUpdateDestination();
  const initial = destination ? providerOf(destination) : "aws";
  const [provider, setProvider] = useState<Provider>(initial);
  const [name, setName] = useState(destination?.name ?? "Data warehouse");
  const [endpoint, setEndpoint] = useState(destination?.endpoint ?? "");
  const [region, setRegion] = useState(destination?.region ?? providers.aws.region);
  const [bucket, setBucket] = useState(destination?.bucket ?? "");
  const [prefix, setPrefix] = useState(destination?.prefix ?? "dumbmail/");
  const [accessKey, setAccessKey] = useState("");
  const [secretKey, setSecretKey] = useState("");
  const [format, setFormat] = useState<Format>(destination?.format ?? "jsonl");
  const [schedule, setSchedule] = useState<Schedule>(destination?.schedule ?? "daily");
  const [chosen, setChosen] = useState<string[] | null>(destination?.datasets ?? null);
  const [error, setError] = useState<string | null>(null);
  const all = datasets.data ?? [];
  const selected = chosen ?? all.map((d) => d.name);
  const p = providers[provider];

  const pickProvider = (next: Provider) => {
    setProvider(next);
    setEndpoint(providers[next].endpoint);
    setRegion(providers[next].region);
  };

  const onSubmit = (e: FormEvent) => {
    e.preventDefault();
    setError(null);
    if (selected.length === 0) {
      setError("Pick at least one dataset.");
      return;
    }
    const storage = { endpoint: provider === "aws" ? "" : endpoint.trim(), region: region.trim(), bucket: bucket.trim(), prefix: prefix.trim() };
    const input: DestinationInput = { name: name.trim(), format, schedule, datasets: selected };
    // Only send storage settings that changed: any change there is checked with a test write.
    if (!destination || storage.endpoint !== (destination.endpoint ?? "")) input.endpoint = storage.endpoint;
    if (!destination || storage.region !== destination.region) input.region = storage.region;
    if (!destination || storage.bucket !== destination.bucket) input.bucket = storage.bucket;
    if (!destination || storage.prefix !== destination.prefix) input.prefix = storage.prefix;
    if (accessKey.trim()) input.access_key_id = accessKey.trim();
    if (secretKey.trim()) input.secret_access_key = secretKey.trim();
    const onError = (err: unknown) => setError(errorMessage(err));
    if (destination) {
      update.mutate(
        { id: destination.id, input },
        {
          onSuccess: () => {
            toast.success("Destination saved");
            onClose();
          },
          onError,
        },
      );
    } else {
      create.mutate(input, {
        onSuccess: () => {
          toast.success("Destination added. DumbMail wrote a test file and removed it.");
          onClose();
        },
        onError,
      });
    }
  };

  return (
    <Modal title={destination ? "Edit destination" : "Add destination"} onClose={onClose} size="lg">
      <form onSubmit={onSubmit} className="space-y-4">
        <div className="grid gap-4 sm:grid-cols-2">
          <Field label="Storage service">
            <Select value={provider} onChange={(e) => pickProvider(e.target.value as Provider)}>
              {(Object.keys(providers) as Provider[]).map((k) => (
                <option key={k} value={k}>
                  {providers[k].label}
                </option>
              ))}
            </Select>
          </Field>
          <Field label="Name">
            <Input value={name} onChange={(e) => setName(e.target.value)} required maxLength={100} />
          </Field>
        </div>
        {provider !== "aws" ? (
          <Field label="Endpoint" hint="The service's S3 endpoint (https://).">
            <Input value={endpoint} onChange={(e) => setEndpoint(e.target.value)} required type="url" placeholder={p.endpoint} />
          </Field>
        ) : null}
        <div className="grid gap-4 sm:grid-cols-3">
          <Field label="Bucket">
            <Input value={bucket} onChange={(e) => setBucket(e.target.value)} required placeholder="acme-data" />
          </Field>
          <Field label="Folder" hint="Optional prefix.">
            <Input value={prefix} onChange={(e) => setPrefix(e.target.value)} placeholder="dumbmail/" />
          </Field>
          <Field label="Region" hint={p.regionHint}>
            <Input value={region} onChange={(e) => setRegion(e.target.value)} required />
          </Field>
        </div>
        <div className="grid gap-4 sm:grid-cols-2">
          <Field label="Access key ID" hint={destination ? `Now ${destination.access_key_hint}. Leave blank to keep it.` : p.keyHint}>
            <Input value={accessKey} onChange={(e) => setAccessKey(e.target.value)} required={!destination} autoComplete="off" />
          </Field>
          <Field label="Secret access key" hint={destination ? "Leave blank to keep it." : "Stored encrypted and never shown again."}>
            <Input value={secretKey} onChange={(e) => setSecretKey(e.target.value)} required={!destination} type="password" autoComplete="new-password" />
          </Field>
        </div>
        <div className="grid gap-4 sm:grid-cols-2">
          <Field label="Schedule">
            <Segmented
              value={schedule}
              onChange={setSchedule}
              options={[
                { value: "hourly", label: "Hourly" },
                { value: "daily", label: "Daily" },
                { value: "manual", label: "Manual" },
              ]}
            />
          </Field>
          <Field label="File format">
            <Segmented
              value={format}
              onChange={setFormat}
              options={[
                { value: "jsonl", label: "JSON lines" },
                { value: "csv", label: "CSV" },
              ]}
            />
          </Field>
        </div>
        <div>
          <div className="mb-1.5 text-sm font-medium text-zinc-800">Datasets</div>
          <div className="grid gap-2 rounded-lg border border-zinc-200 p-3 sm:grid-cols-2">
            {all.map((d) => (
              <Checkbox
                key={d.name}
                checked={selected.includes(d.name)}
                onChange={(e) => setChosen(e.target.checked ? [...selected, d.name] : selected.filter((x) => x !== d.name))}
                label={
                  <span>
                    <code className="text-xs">{d.name}</code> <span className="text-xs text-zinc-400">{d.mode === "incremental" ? "changes" : "whole table"}</span>
                  </span>
                }
                description={d.description}
              />
            ))}
          </div>
        </div>
        <p className="text-xs text-zinc-500">DumbMail checks it can write to the bucket (a small test file it then deletes) before saving.</p>
        {error ? <p className="text-sm text-red-700">{error}</p> : null}
        <div className="flex justify-end gap-2">
          <Button variant="ghost" onClick={onClose}>
            Cancel
          </Button>
          <Button type="submit" variant="primary" loading={create.isPending || update.isPending}>
            {destination ? "Save" : "Add destination"}
          </Button>
        </div>
      </form>
    </Modal>
  );
}
