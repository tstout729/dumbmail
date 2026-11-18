import { useState } from "react";
import { FlaskConical, Play, Plus, X } from "lucide-react";
import { errorMessage } from "../../api/client";
import { useSession } from "../../api";
import { Badge, Button, Card, ConfirmDialog, Field, IconButton, InlineError, Input, Textarea } from "../../components/ui";
import { formatDateTime, formatRelative } from "../../lib/format";
import { toast } from "../../lib/toast";
import { useCancelBackfill, useRecipes, useResumeBackfill, useRunPull, useTestIntegration, useUpdateIntegration } from "./api";
import { monoTextarea, parseJson } from "./parts";
import { TestResultView } from "./ScriptTab";
import type { Integration } from "./types";

const EXAMPLE = `{
  "url": "https://api.example.com/v1/orders",
  "query": { "updated_since": "{{state.cursor|since}}", "limit": 100 },
  "auth": { "type": "bearer", "token": "{{secret.api_key}}" },
  "interval_minutes": 15
}`;

export function PullTab({ integration: i }: { integration: Integration }) {
  return (
    <div className="grid min-w-0 gap-4 lg:grid-cols-2">
      <PullSettings integration={i} />
      <div className="min-w-0 space-y-4">
        <Credentials integration={i} />
        {i.pull ? <RunCard integration={i} /> : null}
      </div>
    </div>
  );
}

function PullSettings({ integration: i }: { integration: Integration }) {
  const save = useUpdateIntegration(i.id);
  const [text, setText] = useState(i.pull ? JSON.stringify(i.pull, null, 2) : "");
  const [error, setError] = useState<string | null>(null);
  const [removing, setRemoving] = useState(false);
  const onSave = () => {
    const parsed = parseJson(text);
    if (!parsed.ok) return setError(parsed.error);
    setError(null);
    save.mutate({ pull: parsed.value as Record<string, unknown> }, { onSuccess: () => toast.success("Pull settings saved") });
  };
  return (
    <Card
      title="Pull settings"
      actions={
        i.pull ? (
          <Button size="sm" variant="ghost" onClick={() => setRemoving(true)}>
            Remove pull
          </Button>
        ) : null
      }
    >
      <p className="mb-2 text-sm text-zinc-600">
        For apps without webhooks, or to import history: DumbMail fetches this request on a schedule and runs the script on each answer. Use <code className="rounded bg-zinc-100 px-1 text-xs">{"{{secret.NAME}}"}</code> for credentials,{" "}
        <code className="rounded bg-zinc-100 px-1 text-xs">{"{{state.KEY}}"}</code> for a saved cursor and <code className="rounded bg-zinc-100 px-1 text-xs">{"{{since}}"}</code> for the start time.
      </p>
      <Textarea wrap="off" aria-label="Pull settings" value={text} onChange={(e) => setText(e.target.value)} rows={18} spellCheck={false} placeholder={EXAMPLE} className={monoTextarea} />
      <InlineError message={error ?? (save.error ? errorMessage(save.error) : null)} />
      <div className="mt-2 flex justify-end">
        <Button size="sm" variant="primary" loading={save.isPending} onClick={onSave} disabled={!text.trim()}>
          Save pull settings
        </Button>
      </div>
      {removing ? (
        <ConfirmDialog
          title="Remove the pull?"
          body="Scheduled pulls stop. Webhooks keep working."
          confirmLabel="Remove pull"
          loading={save.isPending}
          onClose={() => setRemoving(false)}
          onConfirm={() => save.mutate({ pull: null }, { onSuccess: () => setRemoving(false) })}
        />
      ) : null}
    </Card>
  );
}

function Credentials({ integration: i }: { integration: Integration }) {
  const save = useUpdateIntegration(i.id);
  const recipes = useRecipes();
  const labels = new Map((recipes.data ?? []).find((r) => r.key === i.recipe)?.credentials.map((c) => [c.name, c] as const) ?? []);
  const [added, setAdded] = useState<string[]>([]);
  const names = Array.from(new Set([...Object.keys(i.credentials ?? {}), ...i.missing_credentials, ...labels.keys(), ...added]));
  const [values, setValues] = useState<Record<string, string>>({});
  const [extra, setExtra] = useState("");
  const changed = Object.entries(values).filter(([, v]) => v.trim() !== "");
  const onSave = () => {
    save.mutate(
      { credentials: Object.fromEntries(changed) },
      {
        onSuccess: () => {
          setValues({});
          toast.success("Credentials saved");
        },
      },
    );
  };
  return (
    <Card title="Credentials">
      <p className="mb-3 text-sm text-zinc-600">Secrets the pull uses. They're stored encrypted and never shown again; only the last few characters are.</p>
      <div className="space-y-3">
        {names.map((n) => {
          const saved = (i.credentials ?? {})[n] as string | undefined;
          return (
            <div key={n} className="flex items-end gap-2">
              <Field
                className="min-w-0 flex-1"
                label={
                  <span className="flex items-center gap-2">
                    {labels.get(n)?.label ?? n}
                    {saved ? <Badge tone="green">Saved {saved}</Badge> : <Badge tone="amber">Missing</Badge>}
                  </span>
                }
                hint={labels.get(n)?.help}
              >
                <Input type="password" autoComplete="off" value={values[n] ?? ""} onChange={(e) => setValues({ ...values, [n]: e.target.value })} placeholder={saved ? "Type to replace" : "Paste the value"} />
              </Field>
              {saved ? (
                <IconButton label={`Remove ${n}`} onClick={() => save.mutate({ credentials: { [n]: null } })}>
                  <X className="size-4" />
                </IconButton>
              ) : null}
            </div>
          );
        })}
        <div className="flex items-end gap-2">
          <Field className="min-w-0 flex-1" label="Add another" hint="Letters, digits, - and _. Use it as {{secret.NAME}}.">
            <Input value={extra} onChange={(e) => setExtra(e.target.value.replace(/[^A-Za-z0-9_-]/g, ""))} placeholder="api_key" />
          </Field>
          <IconButton
            label="Add credential"
            onClick={() => {
              if (extra && !names.includes(extra)) setAdded([...added, extra]);
              setExtra("");
            }}
          >
            <Plus className="size-4" />
          </IconButton>
        </div>
      </div>
      <InlineError message={save.error ? errorMessage(save.error) : null} />
      <div className="mt-3 flex justify-end">
        <Button size="sm" variant="primary" loading={save.isPending} disabled={changed.length === 0} onClick={onSave}>
          Save credentials
        </Button>
      </div>
    </Card>
  );
}

function RunCard({ integration: i }: { integration: Integration }) {
  const { account } = useSession();
  const run = useRunPull(i.id);
  const cancel = useCancelBackfill(i.id);
  const resume = useResumeBackfill(i.id);
  const test = useTestIntegration(i.id);
  const [since, setSince] = useState("");
  const [confirm, setConfirm] = useState<"now" | "backfill" | null>(null);
  const b = i.backfill;
  const interval = typeof i.pull?.interval_minutes === "number" ? i.pull.interval_minutes : null;
  const start = (what: "now" | "backfill") =>
    run.mutate(what === "backfill" ? since : undefined, {
      onSuccess: () => {
        setConfirm(null);
        toast.success(what === "backfill" ? "Backfill started" : "Pull started");
      },
      onError: (e) => {
        setConfirm(null);
        toast.error(errorMessage(e));
      },
    });
  return (
    <Card title="Run">
      <p className="text-sm text-zinc-600">
        {interval ? `Runs every ${interval} minutes` : "Runs only when you start it"}
        {interval && i.pull_next_at && i.status === "enabled" ? `; next ${formatRelative(i.pull_next_at)}` : ""}.
        {i.oauth_token_cached_until ? ` Access token reused until ${formatDateTime(i.oauth_token_cached_until, account.timezone)}.` : ""}
      </p>
      <div className="mt-3 flex flex-wrap gap-2">
        <Button size="sm" variant="secondary" icon={<FlaskConical className="size-3.5" />} loading={test.isPending} onClick={() => test.mutate({ pull: true })}>
          Test pull
        </Button>
        <Button size="sm" variant="secondary" icon={<Play className="size-3.5" />} onClick={() => setConfirm("now")}>
          Pull now
        </Button>
      </div>
      {test.error ? <InlineError message={errorMessage(test.error)} /> : null}
      {test.data ? <TestResultView result={test.data} /> : null}
      <div className="mt-5 border-t border-zinc-100 pt-4">
        <div className="text-sm font-medium text-zinc-900">Import history</div>
        <p className="mb-2 text-xs text-zinc-500">Runs the same pull from a start date, page by page, until it's done.</p>
        {b?.status === "running" ? (
          <div className="flex flex-wrap items-center justify-between gap-2 rounded-md bg-sky-50 px-3 py-2 text-sm text-sky-900">
            <span>
              Importing since {b.since.slice(0, 10)}: {b.pages} {b.pages === 1 ? "page" : "pages"} so far
            </span>
            <Button size="sm" variant="secondary" loading={cancel.isPending} onClick={() => cancel.mutate(undefined)}>
              Cancel
            </Button>
          </div>
        ) : (
          <div className="flex flex-wrap items-end gap-2">
            <Field label="Start date" className="w-44">
              <Input type="date" value={since} onChange={(e) => setSince(e.target.value)} max={new Date().toISOString().slice(0, 10)} />
            </Field>
            <Button size="sm" variant="secondary" disabled={!since} onClick={() => setConfirm("backfill")}>
              Start import
            </Button>
          </div>
        )}
        {b && b.status !== "running" ? (
          <p className="mt-2 text-xs text-zinc-500">
            Last import (since {b.since.slice(0, 10)}): {b.status === "capped" ? "stopped at its page limit" : b.status}, {b.pages} {b.pages === 1 ? "page" : "pages"}
            {b.finished_at ? `, ${formatRelative(b.finished_at)}` : ""}
            {b.error ? `. ${b.error}` : ""}
            {b.note && b.status !== "capped" ? `. ${b.note}` : ""}
          </p>
        ) : null}
        {b && (b.status === "capped" || b.status === "failed") ? (
          <div className="mt-2">
            <Button
              size="sm"
              variant="secondary"
              loading={resume.isPending}
              onClick={() =>
                resume.mutate(undefined, {
                  onSuccess: () => toast.success("Import resumed"),
                  onError: (e) => toast.error(errorMessage(e)),
                })
              }
            >
              Resume import
            </Button>
            <p className="mt-1 text-xs text-zinc-500">Picks up from the last page it finished.</p>
          </div>
        ) : null}
      </div>
      {confirm ? (
        <ConfirmDialog
          title={confirm === "backfill" ? `Import everything since ${since}?` : "Pull now?"}
          body="This writes profiles and events, and flows triggered by those events can send messages. Try Test pull first to see what would be written."
          confirmLabel={confirm === "backfill" ? "Start import" : "Pull now"}
          loading={run.isPending}
          onClose={() => setConfirm(null)}
          onConfirm={() => start(confirm)}
        />
      ) : null}
    </Card>
  );
}
