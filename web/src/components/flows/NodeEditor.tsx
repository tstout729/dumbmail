import { useState } from "react";
import { Pencil, Plus, Trash2, X } from "lucide-react";
import type { DelayUnit, FlowNode, FlowTrigger, List } from "../../api";
import { DelayEditor, EventConditionEditor, NotifyEditor } from "../../features/flowsPlus";
import { EmailPreview } from "../email/EmailPreview";
import { SmsEditor } from "../email/SmsEditor";
import { PushEditor } from "../../features/push/PushEditor";
import { ConditionBuilder } from "../segments/ConditionBuilder";
import { ObjectSplitFields } from "../../features/objects/ObjectSplitFields";
import { Button, Field, IconButton, Input, Segmented, Select, Textarea } from "../ui";

type Props = {
  node: FlowNode;
  lists: List[];
  onChange: (n: FlowNode) => void;
  onEditEmail: () => void;
  readOnly?: boolean;
  /** The flow's trigger (for trigger splits and dates from the event). */
  trigger?: FlowTrigger;
};

export function NodeEditor({ node, lists, onChange, onEditEmail, readOnly, trigger }: Props) {
  switch (node.type) {
    case "delay":
      return <DelayEditor node={node} onChange={onChange} trigger={trigger} readOnly={readOnly} />;
    case "email":
      return (
        <div className="space-y-4">
          <Field label="Step name">
            <Input value={node.name ?? ""} onChange={(e) => onChange({ ...node, name: e.target.value || undefined })} disabled={readOnly} placeholder="Email" />
          </Field>
          <div className="rounded-lg border border-zinc-200 p-3">
            <div className="text-xs text-zinc-500">Subject</div>
            <div className="text-sm font-medium text-zinc-900">{node.content.subject || "No subject yet"}</div>
            {node.content.preheader ? <div className="mt-0.5 text-xs text-zinc-500">{node.content.preheader}</div> : null}
          </div>
          <Button variant="primary" icon={<Pencil className="size-3.5" />} onClick={onEditEmail} className="w-full">
            {readOnly ? "View email" : "Edit email"}
          </Button>
          <EmailPreview content={node.content} className="h-[520px]" />
        </div>
      );
    case "sms":
      return (
        <div className="space-y-4">
          <Field label="Step name">
            <Input value={node.name ?? ""} onChange={(e) => onChange({ ...node, name: e.target.value || undefined })} disabled={readOnly} placeholder="SMS" />
          </Field>
          <SmsEditor value={node.content} onChange={(content) => onChange({ ...node, content })} readOnly={readOnly} templateName={node.name} />
        </div>
      );
    case "push":
      return (
        <div className="space-y-4">
          <Field label="Step name">
            <Input value={node.name ?? ""} onChange={(e) => onChange({ ...node, name: e.target.value || undefined })} disabled={readOnly} placeholder="Push" />
          </Field>
          <PushEditor value={node.content} onChange={(content) => onChange({ ...node, content })} readOnly={readOnly} />
        </div>
      );
    case "condition":
      return (
        <fieldset disabled={readOnly} className="min-w-0 space-y-3">
          <p className="text-sm text-zinc-600">
            People who match go down <b>Yes</b>; everyone else goes down <b>No</b>.
          </p>
          <ConditionBuilder value={node.condition} onChange={(condition) => onChange({ ...node, condition })} />
          <EventConditionEditor node={node} onChange={onChange} trigger={trigger} />
          {trigger?.type === "object" || trigger?.type === "object_date" ? (
            <ObjectSplitFields
              objectType={trigger.object_type}
              filters={node.object_filters}
              onChange={(object_filters) => {
                const next = { ...node, object_filters };
                if (!object_filters) delete next.object_filters;
                onChange(next);
              }}
            />
          ) : null}
        </fieldset>
      );
    case "wait_for_reply":
      return (
        <fieldset disabled={readOnly} className="min-w-0 space-y-3">
          <p className="text-sm text-zinc-600">
            Wait for them to text back. If they reply in time they go down <b>replied</b> right away; otherwise <b>no reply</b> when the time is up. STOP and HELP don't
            count as replies.
          </p>
          <div className="flex gap-2">
            <Input type="number" min={1} value={node.amount} onChange={(e) => onChange({ ...node, amount: Math.max(1, Number(e.target.value) || 1) })} className="w-24" aria-label="Amount" />
            <Select value={node.unit} onChange={(e) => onChange({ ...node, unit: e.target.value as DelayUnit })} className="w-32" aria-label="Unit">
              <option value="minutes">minutes</option>
              <option value="hours">hours</option>
              <option value="days">days</option>
            </Select>
          </div>
        </fieldset>
      );
    case "random_split":
      return <RandomSplitEditor key={node.id} node={node} onChange={onChange} readOnly={readOnly} />;
    case "update_profile":
      return <UpdateProfileEditor key={node.id} node={node} onChange={onChange} readOnly={readOnly} />;
    case "add_to_list":
    case "remove_from_list":
      return (
        <fieldset disabled={readOnly} className="min-w-0 space-y-3">
          <Segmented
            value={node.type}
            onChange={(type) => onChange({ ...node, type })}
            options={[
              { value: "add_to_list", label: "Add to list" },
              { value: "remove_from_list", label: "Remove from list" },
            ]}
          />
          <Field label="List">
            <Select value={node.list_id} onChange={(e) => onChange({ ...node, list_id: e.target.value })}>
              {node.list_id === "" ? <option value="">Choose a list…</option> : null}
              {lists.map((l) => (
                <option key={l.id} value={l.id}>
                  {l.name}
                </option>
              ))}
            </Select>
          </Field>
        </fieldset>
      );
    case "webhook":
      return <WebhookEditor key={node.id} node={node} onChange={onChange} readOnly={readOnly} />;
    case "notify":
      return <NotifyEditor node={node} onChange={onChange} readOnly={readOnly} />;
  }
}

/** A whole number from 1 to 99, or null. */
function parsePercent(raw: string): number | null {
  if (!/^\d+$/.test(raw.trim())) return null;
  const n = Number(raw);
  return n >= 1 && n <= 99 ? n : null;
}

function RandomSplitEditor({ node, onChange, readOnly }: { node: Extract<FlowNode, { type: "random_split" }>; onChange: (n: FlowNode) => void; readOnly?: boolean }) {
  // Kept as text so the field can be cleared or mid-edit; only valid values reach the flow.
  const [draft, setDraft] = useState(() => String(node.percent));
  const valid = parsePercent(draft) !== null;
  return (
    <fieldset disabled={readOnly} className="min-w-0 space-y-3">
      <p className="text-sm text-zinc-600">
        Each person is sent down path <b>A</b> or <b>B</b> at random. Compare the paths' per-step stats, then keep the winner.
      </p>
      <Field label="Path A share (%)" error={valid ? null : "Enter a whole number from 1 to 99."} hint={`Path B gets the other ${100 - node.percent}%.`}>
        <Input
          type="number"
          min={1}
          max={99}
          step={1}
          inputMode="numeric"
          value={draft}
          onChange={(e) => {
            setDraft(e.target.value);
            const percent = parsePercent(e.target.value);
            if (percent !== null) onChange({ ...node, percent });
          }}
          onBlur={() => setDraft(String(node.percent))}
          className="w-24"
        />
      </Field>
    </fieldset>
  );
}

type Kind = "text" | "number" | "boolean";
type Row = { key: string; value: string; kind: Kind };

function toRows(props: Record<string, unknown>): Row[] {
  return Object.entries(props).map(([key, v]) => ({
    key,
    value: typeof v === "string" ? v : JSON.stringify(v),
    kind: typeof v === "number" ? "number" : typeof v === "boolean" ? "boolean" : "text",
  }));
}

function toRecord(rows: Row[]): Record<string, unknown> {
  const out: Record<string, unknown> = {};
  for (const r of rows) {
    if (!r.key.trim()) continue;
    out[r.key.trim()] = r.kind === "number" ? Number(r.value) || 0 : r.kind === "boolean" ? r.value === "true" : r.value;
  }
  return out;
}

function UpdateProfileEditor({ node, onChange, readOnly }: { node: Extract<FlowNode, { type: "update_profile" }>; onChange: (n: FlowNode) => void; readOnly?: boolean }) {
  const [rows, setRows] = useState<Row[]>(() => toRows(node.properties));
  const commit = (next: Row[]) => {
    setRows(next);
    onChange({ ...node, properties: toRecord(next) });
  };
  return (
    <fieldset disabled={readOnly} className="min-w-0 space-y-3">
      <p className="text-sm text-zinc-600">Set custom properties on the profile. Template variables work in text values.</p>
      {rows.map((r, i) => (
        <div key={i} className="flex items-center gap-1.5">
          <Input value={r.key} onChange={(e) => commit(rows.map((x, j) => (j === i ? { ...x, key: e.target.value } : x)))} placeholder="property" className="w-32" aria-label="Property" />
          <Select value={r.kind} onChange={(e) => commit(rows.map((x, j) => (j === i ? { ...x, kind: e.target.value as Kind, value: e.target.value === "boolean" ? "true" : x.value } : x)))} className="w-24" aria-label="Type">
            <option value="text">Text</option>
            <option value="number">Number</option>
            <option value="boolean">Yes/No</option>
          </Select>
          {r.kind === "boolean" ? (
            <Select value={r.value} onChange={(e) => commit(rows.map((x, j) => (j === i ? { ...x, value: e.target.value } : x)))} className="flex-1" aria-label="Value">
              <option value="true">true</option>
              <option value="false">false</option>
            </Select>
          ) : (
            <Input value={r.value} type={r.kind === "number" ? "number" : "text"} onChange={(e) => commit(rows.map((x, j) => (j === i ? { ...x, value: e.target.value } : x)))} placeholder="value" className="flex-1" aria-label="Value" />
          )}
          <IconButton label="Remove" onClick={() => commit(rows.filter((_, j) => j !== i))}>
            <X className="size-3.5" />
          </IconButton>
        </div>
      ))}
      <Button size="sm" variant="ghost" icon={<Plus className="size-3" />} onClick={() => setRows([...rows, { key: "", value: "", kind: "text" }])}>
        Add property
      </Button>
    </fieldset>
  );
}

function WebhookEditor({ node, onChange, readOnly }: { node: Extract<FlowNode, { type: "webhook" }>; onChange: (n: FlowNode) => void; readOnly?: boolean }) {
  const [body, setBody] = useState(() => (node.body === undefined ? "" : JSON.stringify(node.body, null, 2)));
  const [error, setError] = useState<string | null>(null);
  return (
    <fieldset disabled={readOnly} className="min-w-0 space-y-3">
      <div className="flex gap-2">
        <Select value={node.method ?? "POST"} onChange={(e) => onChange({ ...node, method: e.target.value as "POST" | "GET" })} className="w-24" aria-label="Method">
          <option value="POST">POST</option>
          <option value="GET">GET</option>
        </Select>
        <Input value={node.url} onChange={(e) => onChange({ ...node, url: e.target.value })} placeholder="https://example.com/hook" aria-label="URL" />
      </div>
      {(node.method ?? "POST") === "POST" ? (
        <Field label="JSON body (optional)" error={error} hint="Leave blank to send the profile and event. Template variables work in strings.">
          <Textarea
            rows={8}
            value={body}
            onChange={(e) => setBody(e.target.value)}
            onBlur={() => {
              if (!body.trim()) {
                setError(null);
                onChange({ ...node, body: undefined });
                return;
              }
              try {
                onChange({ ...node, body: JSON.parse(body) as unknown });
                setError(null);
              } catch {
                setError("That isn't valid JSON.");
              }
            }}
            className="font-mono text-xs"
            placeholder={'{"email": "{{ person.email }}"}'}
          />
        </Field>
      ) : null}
    </fieldset>
  );
}

export function DeleteStepButton({ onDelete }: { onDelete: () => void }) {
  return (
    <Button variant="danger" size="sm" icon={<Trash2 className="size-3.5" />} onClick={onDelete}>
      Delete step
    </Button>
  );
}
