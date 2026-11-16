import { InlineTitle } from "../../components/ui/InlineTitle";
import { useState } from "react";
import { useBlocker, useParams } from "react-router";
import { Settings2 } from "lucide-react";
import type { Flow, FlowDefinition, FlowNode, FlowStatus, FlowTrigger } from "../../api";
import { useFlow, useFlowReport, useLists, useSegments, useSession, useSetFlowStatus, useUpdateFlow } from "../../api";
import { EmailEditor } from "../../components/email/EmailEditor";
import { FlowCanvas, nodeIcons } from "../../components/flows/FlowCanvas";
import { FlowSettingsEditor } from "../../components/flows/FlowSettingsEditor";
import { countDescendants, insertAt, isBranch, newNode, nodeLabels, removeNode, replaceNode, triggerSummary, type Slot } from "../../components/flows/graph";
import { DeleteStepButton, NodeEditor } from "../../components/flows/NodeEditor";
import { TriggerEditor } from "../../components/flows/TriggerEditor";
import { Button, ConfirmDialog, Drawer, Modal, Page, PageHeader, Segmented, Skeleton, Stat, cx } from "../../components/ui";
import { formatCurrency, formatNumber, formatPercent } from "../../lib/format";
import { toast } from "../../lib/toast";

export function FlowBuilderPage() {
  const { id = "" } = useParams();
  const flow = useFlow(id);
  if (flow.isPending) {
    return (
      <Page>
        <Skeleton className="mb-6 h-7 w-64" />
        <div className="flex flex-col items-center gap-6">
          {[0, 1, 2, 3].map((i) => (
            <Skeleton key={i} className="h-16 w-[300px]" />
          ))}
        </div>
      </Page>
    );
  }
  if (flow.error) {
    return (
      <Page>
        <PageHeader title="Flow" breadcrumbs={[{ label: "Flows", to: "/flows" }]} />
        <p className="text-sm text-red-600">{flow.error.message}</p>
      </Page>
    );
  }
  return <FlowBuilder key={flow.data.id} flow={flow.data} />;
}

type Draft = { name: string; trigger: FlowTrigger; definition: FlowDefinition };
type Panel = { kind: "trigger" } | { kind: "settings" } | { kind: "node"; id: string } | null;

const panelWidths: Partial<Record<FlowNode["type"], string>> = {
  condition: "w-[min(720px,100vw)]",
  sms: "w-[min(720px,100vw)]",
  email: "w-[min(560px,100vw)]",
};

function FlowBuilder({ flow }: { flow: Flow }) {
  const { account } = useSession();
  const lists = useLists();
  const segments = useSegments();
  const update = useUpdateFlow(flow.id);
  const setStatus = useSetFlowStatus();
  const report = useFlowReport(flow.id, flow.status !== "draft");
  const [draft, setDraft] = useState<Draft | null>(null);
  const [panel, setPanel] = useState<Panel>(null);
  const [emailNodeId, setEmailNodeId] = useState<string | null>(null);
  const [pendingDelete, setPendingDelete] = useState<FlowNode | null>(null);

  const current: Draft = draft ?? { name: flow.name, trigger: flow.trigger, definition: flow.definition };
  const def = current.definition;
  const dirty = draft !== null;
  const listData = lists.data ?? [];
  const blocker = useBlocker(({ currentLocation, nextLocation }) => dirty && currentLocation.pathname !== nextLocation.pathname);

  const edit = (patch: Partial<Draft>) => setDraft({ ...current, ...patch });
  const editDef = (definition: FlowDefinition) => edit({ definition });

  const save = async () => {
    if (!draft) return true;
    try {
      await update.mutateAsync(draft);
      setDraft(null);
      return true;
    } catch {
      return false;
    }
  };

  const changeStatus = async (status: FlowStatus) => {
    if (status === flow.status) return;
    if (!(await save())) return;
    setStatus.mutate(
      { id: flow.id, status },
      { onSuccess: () => toast.success(status === "live" ? "Flow is live" : status === "paused" ? "Flow paused" : "Flow set to draft") },
    );
  };

  const insert = (slot: Slot, type: FlowNode["type"]) => {
    const node = newNode(type, account.brand_color, listData[0]?.id);
    editDef(insertAt(def, slot, node));
    setPanel({ kind: "node", id: node.id });
  };

  const doDelete = (node: FlowNode) => {
    editDef(removeNode(def, node.id));
    setPanel(null);
    setPendingDelete(null);
  };

  const requestDelete = (node: FlowNode) => {
    if (isBranch(node) && countDescendants(def, node.no) > 0) setPendingDelete(node);
    else doDelete(node);
  };

  const selectedNode = panel?.kind === "node" ? def.nodes.find((n) => n.id === panel.id) : undefined;
  const emailNode = emailNodeId ? def.nodes.find((n) => n.id === emailNodeId) : undefined;
  const stats = report.data?.stats ?? flow.stats;

  return (
    <div className="flex h-[calc(100dvh-3rem)] flex-col lg:h-dvh">
      <div className="border-b border-zinc-200 bg-white">
        <Page wide>
          <PageHeader
            breadcrumbs={[{ label: "Flows", to: "/flows" }]}
            title={
              <InlineTitle value={current.name} onChange={(e) => edit({ name: e.target.value })} aria-label="Flow name" />
            }
            actions={
              <>
                <Segmented
                  value={flow.status}
                  onChange={(s) => void changeStatus(s)}
                  options={[
                    { value: "draft", label: "Draft" },
                    { value: "live", label: <span className={cx(flow.status === "live" && "text-accent-700")}>● Live</span> },
                    { value: "paused", label: "Paused" },
                  ]}
                />
                <Button variant="ghost" icon={<Settings2 className="size-3.5" />} onClick={() => setPanel({ kind: "settings" })}>
                  Settings
                </Button>
                <Button variant={dirty ? "primary" : "secondary"} disabled={!dirty} loading={update.isPending} onClick={() => void save().then((ok) => ok && toast.success("Flow saved"))}>
                  {dirty ? "Save" : "Saved"}
                </Button>
              </>
            }
          />
          {flow.status !== "draft" ? (
            <div className="-mt-1 grid grid-cols-2 gap-3 md:grid-cols-5">
              <Stat label="Recipients" value={formatNumber(stats.recipients)} />
              <Stat label="In flow now" value={formatNumber(flow.stats.active_runs)} sub={`${formatNumber(flow.stats.total_runs)} total entries`} />
              <Stat label="Open rate" value={formatPercent(stats.open_rate)} />
              <Stat label="Click rate" value={formatPercent(stats.click_rate)} />
              <Stat label="Revenue" value={formatCurrency(stats.revenue)} />
            </div>
          ) : null}
        </Page>
      </div>
      <div className="min-h-80 flex-1">
        <FlowCanvas
          definition={def}
          trigger={current.trigger}
          triggerLabel={triggerSummary(current.trigger, listData, segments.data)}
          lists={listData}
          report={report.data}
          selected={panel?.kind === "node" ? panel.id : panel?.kind === "trigger" ? "trigger" : null}
          onSelectTrigger={() => setPanel({ kind: "trigger" })}
          onSelectNode={(nid) => setPanel({ kind: "node", id: nid })}
          onInsert={insert}
        />
      </div>

      {panel?.kind === "trigger" ? (
        <Drawer title="Trigger" onClose={() => setPanel(null)}>
          <TriggerEditor trigger={current.trigger} onChange={(trigger) => edit({ trigger })} lists={listData} segments={segments.data ?? []} />
        </Drawer>
      ) : null}
      {panel?.kind === "settings" ? (
        <Drawer title="Flow settings" onClose={() => setPanel(null)} width="w-[min(720px,100vw)]">
          <FlowSettingsEditor settings={def.settings ?? {}} onChange={(settings) => editDef({ ...def, settings })} />
        </Drawer>
      ) : null}
      {selectedNode ? (
        <Drawer
          title={
            <span className="flex items-center gap-2">
              <NodeIcon node={selectedNode} />
              {nodeLabels[selectedNode.type]}
            </span>
          }
          onClose={() => setPanel(null)}
          width={panelWidths[selectedNode.type]}
          footer={<DeleteStepButton onDelete={() => requestDelete(selectedNode)} />}
        >
          <NodeStats node={selectedNode} report={report.data} />
          <NodeEditor
            key={selectedNode.id}
            node={selectedNode}
            lists={listData}
            onChange={(n) => editDef(replaceNode(def, n))}
            onEditEmail={() => setEmailNodeId(selectedNode.id)}
          />
        </Drawer>
      ) : null}
      {emailNode && emailNode.type === "email" ? (
        <Modal title={emailNode.name || "Email"} onClose={() => setEmailNodeId(null)} size="full">
          <div className="h-full overflow-y-auto p-5">
            <EmailEditor value={emailNode.content} onChange={(content) => editDef(replaceNode(def, { ...emailNode, content }))} templateName={emailNode.name} />
          </div>
        </Modal>
      ) : null}
      {pendingDelete ? (
        <ConfirmDialog
          title="Delete this split?"
          body={
            pendingDelete.type === "random_split"
              ? `Path A takes its place. The ${countDescendants(def, pendingDelete.no)} steps on path B are deleted too.`
              : `The Yes branch takes its place. The ${countDescendants(def, isBranch(pendingDelete) ? pendingDelete.no : null)} steps on the No branch are deleted too.`
          }
          confirmLabel="Delete split"
          danger
          onConfirm={() => doDelete(pendingDelete)}
          onClose={() => setPendingDelete(null)}
        />
      ) : null}
      {blocker.state === "blocked" ? (
        <ConfirmDialog
          title="Leave without saving?"
          body="Your changes to this flow haven't been saved."
          confirmLabel="Discard changes"
          danger
          onConfirm={() => blocker.proceed()}
          onClose={() => blocker.reset()}
        />
      ) : null}
    </div>
  );
}

function NodeIcon({ node }: { node: FlowNode }) {
  const meta = nodeIcons[node.type];
  const Icon = meta.icon;
  return (
    <span className={cx("flex size-6 items-center justify-center rounded-md", meta.tone)}>
      <Icon className="size-3.5" />
    </span>
  );
}

function NodeStats({ node, report }: { node: FlowNode; report: ReturnType<typeof useFlowReport>["data"] }) {
  const n = report?.nodes.find((x) => x.node_id === node.id);
  if (!n) return null;
  const isMessage = node.type === "email" || node.type === "sms";
  const cells: [string, string][] = isMessage
    ? [
        ["Entered", formatNumber(n.entered)],
        ...(node.type === "email" ? ([["Opens", formatPercent(n.stats.open_rate)]] as [string, string][]) : []),
        ["Clicks", formatPercent(n.stats.click_rate)],
        ["Revenue", formatCurrency(n.stats.revenue)],
      ]
    : [["Entered", formatNumber(n.entered)]];
  return (
    <div className="mb-4 flex justify-around gap-2 rounded-lg bg-zinc-50 p-3 text-center">
      {cells.map(([label, value]) => (
        <div key={label}>
          <div className="text-[11px] text-zinc-500">{label}</div>
          <div className="text-sm font-semibold tabular-nums">{value}</div>
        </div>
      ))}
    </div>
  );
}
