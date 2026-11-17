import { useState, type FormEvent } from "react";
import { Link } from "react-router";
import { Bookmark, Eye, Unlink } from "lucide-react";
import type { EmailBlock } from "../../api/types";
import { Button, EmptyState, Field, Input, Modal, Select, Skeleton, cx } from "../../components/ui";
import { toast } from "../../lib/toast";
import { useCreateSavedBlock, useSavedBlocks } from "./api";
import type { DisplayRule, RuleOp } from "./types";

type Subject = "first_name" | "email" | "property" | "event_property" | "event_value" | "advanced";

const subjects: { value: Subject; label: string }[] = [
  { value: "property", label: "Profile property" },
  { value: "first_name", label: "First name" },
  { value: "email", label: "Email address" },
  { value: "event_property", label: "Event property (flows)" },
  { value: "event_value", label: "Event value (flows)" },
  { value: "advanced", label: "Advanced expression" },
];

const ops: { value: RuleOp; label: string; needsValue: boolean }[] = [
  { value: "eq", label: "is", needsValue: true },
  { value: "neq", label: "is not", needsValue: true },
  { value: "contains", label: "contains", needsValue: true },
  { value: "gt", label: "is more than", needsValue: true },
  { value: "lt", label: "is less than", needsValue: true },
  { value: "set", label: "has a value", needsValue: false },
  { value: "not_set", label: "is empty", needsValue: false },
];

function parseField(field: string): { subject: Subject; key: string } {
  if (field === "first_name" || field === "person.first_name") return { subject: "first_name", key: "" };
  if (field === "email" || field === "person.email") return { subject: "email", key: "" };
  if (field === "event.value") return { subject: "event_value", key: "" };
  if (field.startsWith("event.properties.")) return { subject: "event_property", key: field.slice("event.properties.".length) };
  if (field.startsWith("person.properties.")) return { subject: "property", key: field.slice("person.properties.".length) };
  return { subject: "property", key: field };
}

function fieldOf(subject: Subject, key: string): string {
  switch (subject) {
    case "first_name":
      return "first_name";
    case "email":
      return "email";
    case "event_value":
      return "event.value";
    case "event_property":
      return `event.properties.${key}`;
    default:
      return `person.properties.${key}`;
  }
}

/** "Show this block only when…": a simple rule or an advanced expression. */
export function DisplayRuleEditor({ rule, onChange }: { rule: DisplayRule | null | undefined; onChange: (r: DisplayRule | null) => void }) {
  const [open, setOpen] = useState(Boolean(rule));
  if (!open && !rule) {
    return (
      <button type="button" onClick={() => setOpen(true)} className="inline-flex items-center gap-1 text-xs font-medium text-zinc-600 hover:text-zinc-900">
        <Eye className="size-3.5" /> Show to everyone · <span className="underline">Add a display rule</span>
      </button>
    );
  }
  const advanced = rule !== null && rule !== undefined && "expr" in rule;
  const simple = rule && "field" in rule ? rule : { field: "person.properties.", op: "eq" as RuleOp, value: "" };
  const { subject, key } = advanced ? { subject: "advanced" as Subject, key: "" } : parseField(simple.field);
  const needsKey = subject === "property" || subject === "event_property";
  const needsValue = ops.find((o) => o.value === simple.op)?.needsValue ?? true;
  return (
    <div className="space-y-2 rounded-md border border-dashed border-zinc-300 p-2.5">
      <div className="flex items-center justify-between">
        <span className="text-xs font-medium text-zinc-700">Show this block only when</span>
        <button
          type="button"
          className="text-xs text-zinc-500 hover:text-zinc-900"
          onClick={() => {
            onChange(null);
            setOpen(false);
          }}
        >
          Remove rule
        </button>
      </div>
      <div className="flex flex-wrap items-center gap-1.5">
        <Select
          value={subject}
          aria-label="What the rule checks"
          className="w-auto"
          onChange={(e) => {
            const next = e.target.value as Subject;
            if (next === "advanced") onChange({ expr: "" });
            else onChange({ field: fieldOf(next, needsKey ? key : ""), op: simple.op, value: simple.value });
          }}
        >
          {subjects.map((s) => (
            <option key={s.value} value={s.value}>
              {s.label}
            </option>
          ))}
        </Select>
        {advanced ? null : (
          <>
            {needsKey ? (
              <Input value={key} onChange={(e) => onChange({ ...simple, field: fieldOf(subject, e.target.value.trim()) })} placeholder="property name" aria-label="Property name" className="w-36" />
            ) : null}
            <Select value={simple.op} aria-label="Comparison" className="w-auto" onChange={(e) => onChange({ ...simple, op: e.target.value as RuleOp })}>
              {ops.map((o) => (
                <option key={o.value} value={o.value}>
                  {o.label}
                </option>
              ))}
            </Select>
            {needsValue ? <Input value={simple.value ?? ""} onChange={(e) => onChange({ ...simple, value: e.target.value })} placeholder="value" aria-label="Value" className="w-32" /> : null}
          </>
        )}
      </div>
      {advanced && rule && "expr" in rule ? (
        <Field label="Expression" hint="Any template expression, like: event.value > 100 and person.properties.vip">
          <Input value={rule.expr} onChange={(e) => onChange({ expr: e.target.value })} className="font-mono text-xs" />
        </Field>
      ) : null}
      <p className="text-[11px] text-zinc-500">Text comparisons ignore capitals. Use “Preview as” to see the email as a particular person.</p>
    </div>
  );
}

/** Save a block as a reusable saved block, then link this email to it. */
export function SaveAsSavedBlock({ block, onSaved }: { block: EmailBlock; onSaved: (ref: EmailBlock) => void }) {
  const [open, setOpen] = useState(false);
  const create = useCreateSavedBlock();
  const onSubmit = (e: FormEvent<HTMLFormElement>) => {
    e.preventDefault();
    const name = String(new FormData(e.currentTarget).get("name")).trim();
    create.mutate(
      { name, blocks: [block] },
      {
        onSuccess: (s) => {
          onSaved({ type: "saved", saved_block_id: s.id, name: s.name, blocks: s.blocks });
          toast.success("Saved. Use it in any email from Add block → Saved block.");
          setOpen(false);
        },
      },
    );
  };
  return (
    <>
      <Button size="sm" variant="ghost" icon={<Bookmark className="size-3.5" />} onClick={() => setOpen(true)}>
        Save for reuse
      </Button>
      {open ? (
        <Modal title="Save as a saved block" description="Use it in any email. When you edit it, every email using it that hasn't been sent changes too." onClose={() => setOpen(false)} size="sm">
          <form onSubmit={onSubmit} className="space-y-4">
            <Field label="Name">
              <Input name="name" required autoFocus maxLength={100} placeholder="Footer with social links" />
            </Field>
            <div className="flex justify-end gap-2">
              <Button variant="ghost" onClick={() => setOpen(false)}>
                Cancel
              </Button>
              <Button type="submit" variant="primary" loading={create.isPending}>
                Save block
              </Button>
            </div>
          </form>
        </Modal>
      ) : null}
    </>
  );
}

/** Actions for a saved block in an email: edit it everywhere, or detach a copy. */
export function SavedRefActions({ block, onDetach }: { block: Extract<EmailBlock, { type: "saved" }>; onDetach: (blocks: EmailBlock[]) => void }) {
  return (
    <div className="flex flex-wrap gap-1.5">
      {block.saved_block_id && !block.frozen ? (
        <Link to={`/content/saved-blocks/${block.saved_block_id}`} className="inline-flex items-center rounded-md px-2 py-1 text-xs font-medium text-accent-800 hover:bg-accent-50">
          Edit saved block
        </Link>
      ) : null}
      <Button size="sm" variant="ghost" icon={<Unlink className="size-3.5" />} onClick={() => onDetach(block.blocks)}>
        Detach and edit here
      </Button>
    </div>
  );
}

/** Pick a saved block to insert. */
export function SavedBlockPicker({ onPick, onClose }: { onPick: (b: EmailBlock) => void; onClose: () => void }) {
  const saved = useSavedBlocks();
  return (
    <Modal title="Insert a saved block" description="It stays linked: editing the saved block updates this email until it's sent." onClose={onClose} size="lg">
      {saved.isPending ? (
        <div className="space-y-2">
          {[0, 1, 2].map((i) => (
            <Skeleton key={i} className="h-12 w-full" />
          ))}
        </div>
      ) : (saved.data ?? []).length === 0 ? (
        <EmptyState
          icon={<Bookmark className="size-5" />}
          title="No saved blocks yet"
          description="Open any block in an email and choose “Save for reuse”, or create one on the Saved blocks page."
        />
      ) : (
        <ul className="max-h-[60vh] divide-y divide-zinc-100 overflow-y-auto rounded-lg border border-zinc-200">
          {(saved.data ?? []).map((s) => (
            <li key={s.id}>
              <button
                type="button"
                onClick={() => {
                  onPick({ type: "saved", saved_block_id: s.id, name: s.name, blocks: s.blocks });
                  onClose();
                }}
                className={cx("flex w-full items-center justify-between gap-3 px-4 py-3 text-left hover:bg-zinc-50")}
              >
                <span className="min-w-0">
                  <span className="block text-sm font-medium text-zinc-900">{s.name}</span>
                  <span className="block truncate text-xs text-zinc-500">{s.blocks.map((b) => b.type).join(", ")}</span>
                </span>
                <span className="shrink-0 text-xs text-zinc-400">{s.uses ? `In ${s.uses} email${s.uses === 1 ? "" : "s"}` : "Not used yet"}</span>
              </button>
            </li>
          ))}
        </ul>
      )}
    </Modal>
  );
}

/** Under every open block: its display rule, and saving it for reuse (or, for a saved block, editing or detaching it). */
export function BlockOptionsPanel({ block, nested, onChange, onReplace }: { block: EmailBlock; nested?: boolean; onChange: (b: EmailBlock) => void; onReplace: (items: EmailBlock[]) => void }) {
  return (
    <div className="space-y-2 border-t border-zinc-100 pt-3">
      <DisplayRuleEditor rule={block.show_if} onChange={(show_if) => onChange({ ...block, show_if: show_if ?? undefined })} />
      {block.type === "saved" ? (
        <SavedRefActions block={block} onDetach={(items) => onReplace(items.map((b) => (block.show_if && !b.show_if ? { ...b, show_if: block.show_if } : b)))} />
      ) : !nested ? (
        <SaveAsSavedBlock block={block} onSaved={(ref) => onChange({ ...ref, show_if: block.show_if })} />
      ) : null}
    </div>
  );
}
