import { useState } from "react";
import { ChevronDown, Download, History, Loader2 } from "lucide-react";
import { errorMessage, useLists, useSession } from "../../api";
import { Badge, Button, Checkbox, Drawer, Field, InlineError, Input, Menu, Modal, Select, Skeleton } from "../../components/ui";
import { formatDateTime, formatNumber, pluralize } from "../../lib/format";
import { useBulkAction, useBulkCount, useRecentBulk, useStartBulk } from "./api";
import type { BulkAction, BulkActionName, BulkInput, BulkSelection, Channel } from "./types";

const actionLabels: Record<BulkActionName, string> = {
  add_to_list: "Add to list",
  remove_from_list: "Remove from list",
  set_property: "Set a property",
  clear_property: "Clear a property",
  suppress: "Suppress",
  export: "Export as CSV",
  delete: "Delete",
};

const doneLabels: Record<string, string> = {
  add_to_list: "added to the list",
  remove_from_list: "removed from the list",
  set_property: "updated",
  clear_property: "updated",
  suppress: "suppressed",
  export: "exported",
  delete: "deleted",
  suppress_addresses: "suppressed",
};

const destructive: BulkActionName[] = ["remove_from_list", "clear_property", "suppress", "delete"];

type BarProps = {
  /** Profiles picked by hand on this page. */
  selected: Set<string>;
  onClearSelection: () => void;
  /** The filters behind the table (search, list, segment), for "everyone matching". */
  filters: Omit<BulkSelection, "profile_ids">;
  total: number;
  /** IDs on the current page, for "select this page". */
  pageIds: string[];
  onSelectPage: () => void;
};

/** The row above a profiles table: pick some or everyone matching, then run an action on them. */
export function BulkActionsBar({ selected, onClearSelection, filters, total, pageIds, onSelectPage }: BarProps) {
  const [allMatching, setAllMatching] = useState(false);
  const [action, setAction] = useState<BulkActionName | null>(null);
  const [history, setHistory] = useState(false);
  const picking = selected.size > 0 && !allMatching;
  const selection: BulkSelection = picking ? { profile_ids: [...selected] } : filters;
  const scope = picking ? pluralize(selected.size, "selected profile") : `all ${formatNumber(total)} matching`;
  const pageAllSelected = pageIds.length > 0 && pageIds.every((id) => selected.has(id));

  return (
    <div className="mb-3 flex flex-wrap items-center gap-2 text-sm">
      {selected.size > 0 ? (
        <>
          <Badge tone="blue">{allMatching ? `All ${formatNumber(total)} matching` : `${formatNumber(selected.size)} selected`}</Badge>
          {!allMatching && total > selected.size ? (
            <Button size="sm" variant="ghost" onClick={() => setAllMatching(true)}>
              Select all {formatNumber(total)} matching
            </Button>
          ) : null}
          <Button
            size="sm"
            variant="ghost"
            onClick={() => {
              setAllMatching(false);
              onClearSelection();
            }}
          >
            Clear
          </Button>
        </>
      ) : pageIds.length > 0 && !pageAllSelected ? (
        <Button size="sm" variant="ghost" onClick={onSelectPage}>
          Select this page
        </Button>
      ) : null}
      <div className="ml-auto flex items-center gap-2">
        <Button size="sm" variant="ghost" icon={<History className="size-3.5" />} onClick={() => setHistory(true)}>
          Bulk history
        </Button>
        <Menu
          trigger={(props) => (
            <Button size="sm" disabled={total === 0 && selected.size === 0} {...props}>
              Bulk actions <ChevronDown className="size-3.5" />
            </Button>
          )}
          items={(Object.keys(actionLabels) as BulkActionName[]).map((a) => ({ label: `${actionLabels[a]}: ${scope}`, danger: a === "delete", onSelect: () => setAction(a) }))}
        />
      </div>
      {action ? <BulkDialog action={action} selection={selection} onClose={() => setAction(null)} /> : null}
      {history ? <BulkHistory onClose={() => setHistory(false)} /> : null}
    </div>
  );
}

function BulkDialog({ action, selection, onClose }: { action: BulkActionName; selection: BulkSelection; onClose: () => void }) {
  const lists = useLists();
  const [listId, setListId] = useState("");
  const [key, setKey] = useState("");
  const [value, setValue] = useState("");
  const [channels, setChannels] = useState<Channel[]>(["email"]);
  const [typed, setTyped] = useState("");
  const [startedId, setStartedId] = useState<string | null>(null);
  const start = useStartBulk();

  const needsList = action === "add_to_list" || action === "remove_from_list";
  const needsKey = action === "set_property" || action === "clear_property";
  const input: BulkInput = {
    action,
    selection,
    list_id: needsList && listId ? listId : undefined,
    key: needsKey ? key.trim() : undefined,
    value: action === "set_property" ? parseValue(value) : undefined,
    channels: action === "suppress" ? channels : undefined,
  };
  const ready = (!needsList || listId !== "") && (!needsKey || key.trim() !== "") && (action !== "suppress" || channels.length > 0);
  // The count, from the server, of exactly who this would act on now.
  const count = useBulkCount(ready ? input : null);
  const n = count.data?.count ?? null;
  const mustType = destructive.includes(action);
  const typedOk = !mustType || (n !== null && typed.trim().replace(/,/g, "") === String(n));

  if (startedId) return <BulkProgress id={startedId} onClose={onClose} />;

  return (
    <Modal
      title={actionLabels[action]}
      onClose={onClose}
      size="md"
      footer={
        <>
          <Button variant="ghost" onClick={onClose}>
            Cancel
          </Button>
          <Button
            variant={action === "delete" ? "danger" : "primary"}
            disabled={!ready || n === null || n === 0 || !typedOk}
            loading={start.isPending}
            onClick={() => n !== null && start.mutate({ ...input, expected_count: n }, { onSuccess: (b) => setStartedId(b.id) })}
          >
            {n === null ? "Counting…" : `${actionLabels[action]}: ${pluralize(n, "profile")}`}
          </Button>
        </>
      }
    >
      <div className="space-y-3 text-sm">
        {needsList ? (
          <Field label="List">
            <Select value={listId} onChange={(e) => setListId(e.target.value)}>
              <option value="">Choose a list</option>
              {(lists.data ?? []).map((l) => (
                <option key={l.id} value={l.id}>
                  {l.name}
                </option>
              ))}
            </Select>
          </Field>
        ) : null}
        {needsKey ? (
          <div className="grid grid-cols-1 gap-3 sm:grid-cols-2">
            <Field label="Property">
              <Input value={key} onChange={(e) => setKey(e.target.value)} placeholder="e.g. tier" className="font-mono text-xs" maxLength={128} />
            </Field>
            {action === "set_property" ? (
              <Field label="Value" hint="Numbers, true/false and JSON are kept as they are.">
                <Input value={value} onChange={(e) => setValue(e.target.value)} placeholder="e.g. gold" />
              </Field>
            ) : null}
          </div>
        ) : null}
        {action === "suppress" ? (
          <div className="space-y-2">
            <Checkbox label="Email" checked={channels.includes("email")} onChange={(e) => setChannels(e.target.checked ? [...new Set<Channel>([...channels, "email"])] : channels.filter((c) => c !== "email"))} />
            <Checkbox label="Text messages" checked={channels.includes("sms")} onChange={(e) => setChannels(e.target.checked ? [...new Set<Channel>([...channels, "sms"])] : channels.filter((c) => c !== "sms"))} />
            <p className="text-xs text-zinc-500">Suppressed people get no marketing on that channel until someone lifts it on the Suppression list page.</p>
          </div>
        ) : null}
        {action === "add_to_list" ? <p className="text-xs text-zinc-500">Adding someone to a list doesn't subscribe them. List flows start for people new to the list.</p> : null}
        {action === "delete" ? (
          <p className="text-zinc-600">Their events, messages and consent history are deleted too. This can't be undone. People who had unsubscribed or were suppressed stay suppressed if they're imported again.</p>
        ) : null}
        {action === "export" ? <p className="text-zinc-600">You'll get a CSV file to download. It's kept for 7 days.</p> : null}

        <div className="rounded-lg bg-zinc-50 px-3 py-2 text-zinc-700">
          {!ready ? (
            "Fill in the details to see how many profiles this affects."
          ) : count.isFetching && n === null ? (
            <span className="flex items-center gap-2">
              <Loader2 className="size-3.5 animate-spin" /> Counting…
            </span>
          ) : count.error ? (
            <InlineError message={errorMessage(count.error)} />
          ) : (
            <>
              This affects <strong>{pluralize(n ?? 0, "profile")}</strong>. It runs in the background; you can leave this page.
            </>
          )}
        </div>
        {mustType && n !== null && n > 0 ? (
          <Field label={`Type ${formatNumber(n)} to confirm`}>
            <Input value={typed} onChange={(e) => setTyped(e.target.value)} inputMode="numeric" autoFocus aria-label="Number of profiles" />
          </Field>
        ) : null}
        {start.error ? <InlineError message={errorMessage(start.error)} /> : null}
      </div>
    </Modal>
  );
}

function parseValue(raw: string): unknown {
  const t = raw.trim();
  if (/^(-?\d+(\.\d+)?|true|false|null|\[.*\]|\{.*\})$/s.test(t)) {
    try {
      return JSON.parse(t) as unknown;
    } catch {
      return raw;
    }
  }
  return raw;
}

export function BulkProgress({ id, onClose }: { id: string; onClose: () => void }) {
  const job = useBulkAction(id);
  const b = job.data;
  const finished = b?.status === "done" || b?.status === "failed";
  return (
    <Modal title={b ? actionLabels[b.action as BulkActionName] ?? "Suppress addresses" : "Bulk action"} onClose={onClose} size="sm" footer={<Button onClick={onClose}>{finished ? "Done" : "Close"}</Button>}>
      {!b ? <Skeleton className="h-16" /> : <BulkStatus b={b} />}
      {!finished ? <p className="mt-3 text-xs text-zinc-500">You can close this; it keeps running. Find it later under Bulk history.</p> : null}
    </Modal>
  );
}

function BulkStatus({ b }: { b: BulkAction }) {
  const pct = b.total > 0 ? Math.round((b.processed / b.total) * 100) : 100;
  return (
    <div className="space-y-2 text-sm">
      <div className="h-2 overflow-hidden rounded-full bg-zinc-100" role="progressbar" aria-valuenow={pct} aria-valuemin={0} aria-valuemax={100}>
        <div className={b.status === "failed" ? "h-full bg-red-500" : "h-full bg-accent-600"} style={{ width: `${pct}%` }} />
      </div>
      <div className="text-zinc-700">
        {b.status === "done"
          ? `Done. ${formatNumber(b.succeeded)} ${doneLabels[b.action] ?? "changed"}${b.skipped > 0 ? `, ${formatNumber(b.skipped)} already were or no longer exist` : ""}.`
          : b.status === "failed"
            ? `Stopped after ${formatNumber(b.processed)} of ${formatNumber(b.total)}.`
            : `${formatNumber(b.processed)} of ${formatNumber(b.total)} done…`}
      </div>
      {b.error ? <InlineError message={b.error} /> : null}
      {b.status === "done" && b.file_url ? (
        <a href={b.file_url} download className="inline-flex items-center gap-1.5 font-medium text-accent-700 hover:underline">
          <Download className="size-3.5" /> Download CSV
        </a>
      ) : null}
    </div>
  );
}

function BulkHistory({ onClose }: { onClose: () => void }) {
  const recent = useRecentBulk();
  const { account } = useSession();
  return (
    <Drawer title="Bulk history" onClose={onClose}>
      {recent.isPending ? (
        <Skeleton className="h-40" />
      ) : (recent.data?.data ?? []).length === 0 ? (
        <p className="py-8 text-center text-sm text-zinc-500">No bulk actions yet.</p>
      ) : (
        <ul className="space-y-4">
          {(recent.data?.data ?? []).map((b) => (
            <li key={b.id} className="rounded-lg border border-zinc-200 p-3">
              <div className="mb-1 flex items-center justify-between gap-2">
                <span className="text-sm font-medium text-zinc-900">{actionLabels[b.action as BulkActionName] ?? "Suppress addresses"}</span>
                <span className="text-xs text-zinc-500">{formatDateTime(b.created_at, account.timezone)}</span>
              </div>
              <div className="mb-2 text-xs text-zinc-500">{b.created_by ? `By ${b.created_by}` : b.via === "mcp" ? "By the AI assistant" : "By an API key"}</div>
              <BulkStatus b={b} />
            </li>
          ))}
        </ul>
      )}
    </Drawer>
  );
}
