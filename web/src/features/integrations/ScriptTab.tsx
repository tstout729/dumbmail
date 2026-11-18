import { useState } from "react";
import { FlaskConical } from "lucide-react";
import { errorMessage } from "../../api/client";
import { Button, Card, Field, InlineError, Textarea } from "../../components/ui";
import { toast } from "../../lib/toast";
import { useTestIntegration, useUpdateIntegration } from "./api";
import { JsonBlock, countsSummary, monoTextarea, parseJson } from "./parts";
import type { Integration, TestResult } from "./types";

const SAMPLE = `{
  "id": "123",
  "email": "jane@example.com",
  "first_name": "Jane"
}`;

export function ScriptTab({ integration: i }: { integration: Integration }) {
  const save = useUpdateIntegration(i.id);
  const test = useTestIntegration(i.id);
  const [script, setScript] = useState(i.script);
  const [sample, setSample] = useState(SAMPLE);
  const [sampleError, setSampleError] = useState<string | null>(null);
  const dirty = script !== i.script;

  const runTest = () => {
    const parsed = parseJson(sample);
    // Non-JSON samples go in as the raw body text (forms, plain text).
    setSampleError(null);
    test.mutate({ script, payload: parsed.ok ? parsed.value : sample }, { onError: (e) => setSampleError(errorMessage(e)) });
  };

  return (
    <div className="grid min-w-0 gap-4 lg:grid-cols-2">
      <Card
        title="Translation script"
        actions={
          <Button size="sm" variant="primary" disabled={!dirty} loading={save.isPending} onClick={() => save.mutate({ script }, { onSuccess: () => toast.success("Script saved") })}>
            Save script
          </Button>
        }
      >
        <p className="mb-2 text-sm text-zinc-600">
          A JavaScript <code className="rounded bg-zinc-100 px-1 text-xs">handle(input)</code> function that returns a list of actions. It runs in a sandbox: no network, files or timers, 2 seconds at most.
        </p>
        <Textarea wrap="off" aria-label="Script" value={script} onChange={(e) => setScript(e.target.value)} rows={26} spellCheck={false} className={monoTextarea} />
        <InlineError message={save.error ? errorMessage(save.error) : null} />
        {dirty ? <p className="mt-2 text-xs text-amber-700">Unsaved changes. Tests use what's in the editor.</p> : null}
      </Card>
      <div className="min-w-0 space-y-4">
        <Card
          title="Test"
          actions={
            <Button size="sm" variant="secondary" icon={<FlaskConical className="size-3.5" />} loading={test.isPending} onClick={runTest}>
              Run test
            </Button>
          }
        >
          <Field label="Sample payload" hint="Paste a real webhook body from the app's docs. Nothing is written; you see exactly what would be.">
            <Textarea value={sample} onChange={(e) => setSample(e.target.value)} rows={9} spellCheck={false} className={monoTextarea} />
          </Field>
          <InlineError message={sampleError} />
          {test.data ? <TestResultView result={test.data} /> : null}
        </Card>
        <MetricsCard integration={i} />
        <SettingsCard integration={i} />
      </div>
    </div>
  );
}

export function TestResultView({ result: r }: { result: TestResult }) {
  return (
    <div className="mt-4 space-y-3">
      {r.ok ? (
        <div className="rounded-md border border-accent-200 bg-accent-50 px-3 py-2 text-sm text-accent-900">
          <span className="font-medium">Would write:</span> {countsSummary(r.would_write) || "nothing"} <span className="text-xs opacity-75">({r.duration_ms} ms)</span>
        </div>
      ) : (
        <div className="rounded-md border border-red-200 bg-red-50 px-3 py-2 text-sm text-red-800 break-words">{r.error}</div>
      )}
      {r.effects && r.effects.length > 0 ? (
        <div>
          <div className="mb-1 text-xs font-medium text-zinc-700">Step by step</div>
          <ul className="space-y-1 text-sm text-zinc-700">
            {r.effects.map((e, n) => (
              <li key={n} className="break-words rounded bg-zinc-50 px-2 py-1 font-mono text-xs">
                {e}
              </li>
            ))}
          </ul>
        </div>
      ) : null}
      {r.logs && r.logs.length > 0 ? (
        <div>
          <div className="mb-1 text-xs font-medium text-zinc-700">Logs</div>
          <JsonBlock value={r.logs.join("\n")} />
        </div>
      ) : null}
      {r.actions !== undefined ? (
        <details>
          <summary className="cursor-pointer text-xs font-medium text-zinc-700">Actions the script returned</summary>
          <JsonBlock value={r.actions} className="mt-1" />
        </details>
      ) : null}
      {r.pull ? (
        <details>
          <summary className="cursor-pointer text-xs font-medium text-zinc-700">Request and answer</summary>
          <JsonBlock value={r.pull} className="mt-1" />
        </details>
      ) : null}
    </div>
  );
}

/** One entry per line (blank lines and surrounding spaces dropped). */
function lines(text: string): string[] {
  return text
    .split("\n")
    .map((l) => l.trim())
    .filter((l) => l.length > 0);
}

function MetricsCard({ integration: i }: { integration: Integration }) {
  const save = useUpdateIntegration(i.id);
  const [metrics, setMetrics] = useState(i.metrics.join("\n"));
  const [redact, setRedact] = useState(i.redact.join("\n"));
  const dirty = metrics !== i.metrics.join("\n") || redact !== i.redact.join("\n");
  return (
    <Card title="Events and stored payloads">
      <div className="space-y-4">
      <Field label="Events the script records" hint="One per line. The script can record only these, and nobody can send them from a web page, so flows can trust them.">
        <Textarea value={metrics} onChange={(e) => setMetrics(e.target.value)} rows={4} spellCheck={false} className={monoTextarea} />
      </Field>
      <Field
        label="Fields to remove before a payload is kept"
        hint="One per line, like results[].notes or **.keyCode (any depth). Use it for door codes, access notes and anything else that mustn't sit in the delivery log. The script still sees the whole payload."
      >
        <Textarea value={redact} onChange={(e) => setRedact(e.target.value)} rows={4} spellCheck={false} className={monoTextarea} />
      </Field>
      </div>
      <InlineError message={save.error ? errorMessage(save.error) : null} />
      <div className="mt-2 flex justify-end">
        <Button size="sm" variant="secondary" disabled={!dirty} loading={save.isPending} onClick={() => save.mutate({ metrics: lines(metrics), redact: lines(redact) }, { onSuccess: () => toast.success("Saved") })}>
          Save
        </Button>
      </div>
    </Card>
  );
}

function SettingsCard({ integration: i }: { integration: Integration }) {
  const save = useUpdateIntegration(i.id);
  const [settings, setSettings] = useState(JSON.stringify(i.settings, null, 2));
  const [error, setError] = useState<string | null>(null);
  const state = i.state ?? {};
  const hasState = Object.keys(state).length > 0;
  const onSave = () => {
    const parsed = parseJson(settings);
    if (!parsed.ok || typeof parsed.value !== "object" || parsed.value === null || Array.isArray(parsed.value)) {
      setError(parsed.ok ? "Settings must be a JSON object" : parsed.error);
      return;
    }
    setError(null);
    save.mutate({ settings: parsed.value as Record<string, unknown> }, { onSuccess: () => toast.success("Settings saved") });
  };
  return (
    <Card title="Settings and saved state">
      <Field label="Settings" hint="Non-secret values the script reads as input.settings, like a list name.">
        <Textarea value={settings} onChange={(e) => setSettings(e.target.value)} rows={4} spellCheck={false} className={monoTextarea} />
      </Field>
      <InlineError message={error ?? (save.error ? errorMessage(save.error) : null)} />
      <div className="mt-2 flex justify-end">
        <Button size="sm" variant="secondary" loading={save.isPending} onClick={onSave}>
          Save settings
        </Button>
      </div>
      <div className="mt-4">
        <div className="mb-1 flex items-center justify-between gap-2">
          <span className="text-xs font-medium text-zinc-700">Saved state</span>
          {hasState ? (
            <Button size="sm" variant="ghost" onClick={() => save.mutate({ state: {} }, { onSuccess: () => toast.success("Saved state cleared") })}>
              Clear
            </Button>
          ) : null}
        </div>
        {hasState ? <JsonBlock value={state} /> : <p className="text-xs text-zinc-500">Nothing saved. Scripts save cursors here with set_state.</p>}
      </div>
    </Card>
  );
}
