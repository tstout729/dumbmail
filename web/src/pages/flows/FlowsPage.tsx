import { useState, type FormEvent } from "react";
import { useNavigate, useSearchParams } from "react-router";
import { ArrowLeft, Copy, MoreHorizontal, PenLine, Plus, Search, Sparkles, Trash2, Workflow } from "lucide-react";
import type { Flow, FlowBlueprint } from "../../api";
import {
  useBlueprints,
  useCreateFlow,
  useCreateFlowFromBlueprint,
  useDeleteFlow,
  useFlows,
  useLists,
  useMetrics,
  useSegments,
} from "../../api";
import { ChannelIcons, blueprintChannels } from "../../components/flows/channels";
import { triggerKind, triggerSummary } from "../../components/flows/graph";
import {
  Button,
  ConfirmDialog,
  EmptyRow,
  EmptyState,
  Field,
  IconButton,
  Input,
  Menu,
  Modal,
  Page,
  PageHeader,
  RowLink,
  Skeleton,
  SkeletonRows,
  StatusBadge,
  Table,
  Td,
  Th,
} from "../../components/ui";
import { formatCurrency, formatNumber, formatRelative } from "../../lib/format";

export function FlowsPage() {
  const flows = useFlows();
  const lists = useLists();
  const segments = useSegments();
  const [params, setParams] = useSearchParams();
  const creating = params.get("create") === "1";
  const [q, setQ] = useState("");
  const [deleting, setDeleting] = useState<Flow | null>(null);
  const remove = useDeleteFlow();
  const duplicate = useCreateFlow();
  const navigate = useNavigate();
  const rows = (flows.data ?? []).filter((f) => f.name.toLowerCase().includes(q.toLowerCase()));

  const openCreate = (on: boolean) => {
    const next = new URLSearchParams(params);
    if (on) next.set("create", "1");
    else next.delete("create");
    setParams(next, { replace: true });
  };

  return (
    <Page>
      <PageHeader
        title="Flows"
        description="Automated messages triggered by what people do."
        actions={
          <Button variant="primary" icon={<Plus className="size-4" />} onClick={() => openCreate(true)}>
            Create flow
          </Button>
        }
      />
      <div className="mb-4 flex justify-end">
        <div className="relative">
          <Search className="pointer-events-none absolute left-2.5 top-2.5 size-3.5 text-zinc-400" />
          <Input value={q} onChange={(e) => setQ(e.target.value)} placeholder="Search flows" className="w-56 pl-8" />
        </div>
      </div>
      <Table>
        <thead>
          <tr>
            <Th>Flow</Th>
            <Th>Status</Th>
            <Th>Trigger</Th>
            <Th className="text-right">Recipients</Th>
            <Th className="text-right">In flow now</Th>
            <Th className="text-right">Revenue</Th>
            <Th className="w-10" />
          </tr>
        </thead>
        <tbody>
          {flows.isPending ? (
            <SkeletonRows cols={7} />
          ) : rows.length === 0 ? (
            <EmptyRow cols={7}>
              <EmptyState
                icon={<Workflow className="size-5" />}
                title={(flows.data ?? []).length === 0 ? "No flows yet" : "No matches"}
                description="Start from a proven blueprint like a welcome series or abandoned cart."
                action={
                  <Button variant="primary" onClick={() => openCreate(true)}>
                    Browse blueprints
                  </Button>
                }
              />
            </EmptyRow>
          ) : (
            rows.map((f) => (
              <RowLink key={f.id} to={`/flows/${f.id}`}>
                <Td>
                  <div className="font-medium text-zinc-900">{f.name}</div>
                  <div className="text-xs text-zinc-500">Updated {formatRelative(f.updated_at)}</div>
                </Td>
                <Td>
                  <StatusBadge status={f.status} />
                </Td>
                <Td className="max-w-[260px]">
                  <span className="mr-1.5 rounded bg-zinc-100 px-1.5 py-0.5 text-[11px] font-medium text-zinc-600">{triggerKind(f.trigger)}</span>
                  <span className="truncate text-zinc-600">{triggerSummary(f.trigger, lists.data, segments.data)}</span>
                </Td>
                <Td className="text-right tabular-nums">{formatNumber(f.stats.recipients)}</Td>
                <Td className="text-right tabular-nums">{formatNumber(f.stats.active_runs)}</Td>
                <Td className="text-right font-medium tabular-nums">{formatCurrency(f.stats.revenue)}</Td>
                <Td>
                  <Menu
                    trigger={(props) => (
                      <IconButton label="Flow actions" {...props}>
                        <MoreHorizontal className="size-4" />
                      </IconButton>
                    )}
                    items={[
                      {
                        label: "Duplicate",
                        icon: <Copy className="size-3.5" />,
                        onSelect: () =>
                          duplicate.mutate(
                            { name: `${f.name} (copy)`, trigger: f.trigger, definition: f.definition },
                            { onSuccess: (d) => navigate(`/flows/${d.id}`) },
                          ),
                      },
                      { label: "Delete", icon: <Trash2 className="size-3.5" />, danger: true, onSelect: () => setDeleting(f) },
                    ]}
                  />
                </Td>
              </RowLink>
            ))
          )}
        </tbody>
      </Table>
      {creating ? <CreateFlowDialog onClose={() => openCreate(false)} /> : null}
      {deleting ? (
        <ConfirmDialog
          title={`Delete "${deleting.name}"?`}
          body={deleting.status === "live" ? "This flow is live. People in it will stop receiving its messages." : "This can't be undone."}
          confirmLabel="Delete flow"
          danger
          onConfirm={() => {
            remove.mutate(deleting.id);
            setDeleting(null);
          }}
          onClose={() => setDeleting(null)}
        />
      ) : null}
    </Page>
  );
}

function CreateFlowDialog({ onClose }: { onClose: () => void }) {
  const blueprints = useBlueprints();
  const fromBlueprint = useCreateFlowFromBlueprint();
  const create = useCreateFlow();
  const metrics = useMetrics();
  const navigate = useNavigate();
  const [scratch, setScratch] = useState(false);
  const [pending, setPending] = useState<string | null>(null);

  const pick = (b: FlowBlueprint) => {
    setPending(b.key);
    fromBlueprint.mutate({ key: b.key, name: b.name }, { onSuccess: (f) => navigate(`/flows/${f.id}`), onSettled: () => setPending(null) });
  };

  const onScratch = (e: FormEvent<HTMLFormElement>) => {
    e.preventDefault();
    const d = new FormData(e.currentTarget);
    create.mutate(
      {
        name: String(d.get("name")).trim(),
        trigger: { type: "metric", metric: String(d.get("metric")).trim() },
        definition: { entry: null, nodes: [], settings: { reentry: "never" } },
      },
      { onSuccess: (f) => navigate(`/flows/${f.id}`) },
    );
  };

  return (
    <Modal title={scratch ? "Start from scratch" : "Create a flow"} description={scratch ? undefined : "Pick a blueprint or build your own."} onClose={onClose} size={scratch ? "md" : "xl"}>
      {scratch ? (
        <form onSubmit={onScratch} className="space-y-4">
          <Field label="Flow name">
            <Input name="name" required autoFocus placeholder="Post-purchase thank you" />
          </Field>
          <Field label="Trigger metric" hint="What someone does to enter. You can change this, or use a list, segment or date instead, in the builder.">
            <Input name="metric" required list="pl-flow-metrics" defaultValue="Placed Order" />
            <datalist id="pl-flow-metrics">
              {(metrics.data ?? []).map((m) => (
                <option key={m.id} value={m.name} />
              ))}
            </datalist>
          </Field>
          <div className="flex justify-between gap-2">
            <Button variant="ghost" icon={<ArrowLeft className="size-3.5" />} onClick={() => setScratch(false)}>
              Blueprints
            </Button>
            <Button type="submit" variant="primary" loading={create.isPending}>
              Create flow
            </Button>
          </div>
        </form>
      ) : (
        <div className="grid max-h-[65vh] gap-3 overflow-y-auto sm:grid-cols-2 lg:grid-cols-3">
          <button
            type="button"
            onClick={() => setScratch(true)}
            className="flex flex-col items-start rounded-lg border border-dashed border-zinc-300 p-4 text-left hover:border-zinc-400 hover:bg-zinc-50"
          >
            <PenLine className="size-4 text-zinc-500" />
            <div className="mt-2 text-sm font-semibold text-zinc-900">Start from scratch</div>
            <p className="mt-1 text-xs text-zinc-500">Pick a trigger and build every step yourself.</p>
          </button>
          {blueprints.isPending
            ? [0, 1, 2, 3, 4].map((i) => <Skeleton key={i} className="h-32" />)
            : (blueprints.data ?? []).map((b) => (
                <button
                  key={b.key}
                  type="button"
                  disabled={pending !== null}
                  onClick={() => pick(b)}
                  className="flex flex-col items-start rounded-lg border border-zinc-200 p-4 text-left transition-colors hover:border-accent-400 hover:bg-accent-50/40 disabled:opacity-60"
                >
                  <div className="flex w-full items-center justify-between">
                    <Sparkles className="size-4 text-accent-600" />
                    <ChannelIcons channels={blueprintChannels(b)} />
                  </div>
                  <div className="mt-2 text-sm font-semibold text-zinc-900">{b.name}</div>
                  <p className="mt-1 flex-1 text-xs leading-relaxed text-zinc-600">{b.description}</p>
                  <div className="mt-2 text-[11px] text-zinc-400">
                    {pending === b.key ? "Creating…" : `${b.definition.nodes.length} steps · ${triggerSummary(b.trigger)}`}
                  </div>
                </button>
              ))}
          {blueprints.error ? <p className="text-sm text-red-600">{blueprints.error.message}</p> : null}
        </div>
      )}
    </Modal>
  );
}
