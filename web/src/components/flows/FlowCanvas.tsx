import {
  AlertTriangle,
  Bell,
  Clock,
  CornerDownRight,
  GitBranch,
  ListMinus,
  ListPlus,
  Mail,
  MessageSquare,
  MessageSquareReply,
  Plus,
  Shuffle,
  Star,
  UserCog,
  Webhook,
  Zap,
  type LucideIcon,
} from "lucide-react";
import type { ConditionNode, FlowDefinition, FlowNode, FlowNodeType, FlowReport, FlowTrigger, List, MessageStats, RandomSplitNode, WaitForReplyNode } from "../../api";
import { delaySummary, eventSplitSummary, notifySummary, StepResultLine, type StepResult } from "../../features/flowsPlus";
import { formatCurrency, formatNumber, formatPercent } from "../../lib/format";
import { definitionSummary } from "../segments/conditions";
import { reviewStepSummary } from "../../features/reviews/StepEditor";
import { Menu, cx } from "../ui";
import { getSlot, insertableTypes, isBranch, nodeLabels, type Slot } from "./graph";
import { PanZoom } from "./PanZoom";

export const nodeIcons: Record<FlowNodeType, { icon: LucideIcon; tone: string }> = {
  delay: { icon: Clock, tone: "bg-zinc-100 text-zinc-600" },
  email: { icon: Mail, tone: "bg-sky-50 text-sky-700" },
  sms: { icon: MessageSquare, tone: "bg-violet-50 text-violet-700" },
  condition: { icon: GitBranch, tone: "bg-amber-50 text-amber-700" },
  random_split: { icon: Shuffle, tone: "bg-fuchsia-50 text-fuchsia-700" },
  wait_for_reply: { icon: MessageSquareReply, tone: "bg-violet-50 text-violet-700" },
  update_profile: { icon: UserCog, tone: "bg-zinc-100 text-zinc-600" },
  add_to_list: { icon: ListPlus, tone: "bg-accent-50 text-accent-700" },
  remove_from_list: { icon: ListMinus, tone: "bg-red-50 text-red-700" },
  webhook: { icon: Webhook, tone: "bg-zinc-100 text-zinc-600" },
  notify: { icon: Bell, tone: "bg-amber-50 text-amber-700" },
  review_request: { icon: Star, tone: "bg-amber-50 text-amber-700" },
};

type NodeStats = Map<string, { entered: number; stats: MessageStats }>;

type CanvasProps = {
  definition: FlowDefinition;
  trigger: FlowTrigger;
  triggerLabel: string;
  lists: List[];
  report: FlowReport | undefined;
  /** Per-step results (features/flowsPlus); shown instead of `report` when present. */
  results?: StepResult[];
  /** Open the list of people waiting at a step. */
  onWaiting?: (nodeId: string) => void;
  selected: string | null;
  readOnly?: boolean;
  onSelectTrigger: () => void;
  onSelectNode: (id: string) => void;
  onInsert: (slot: Slot, type: FlowNodeType) => void;
};

export function FlowCanvas(props: CanvasProps) {
  const { definition, triggerLabel, selected, onSelectTrigger, report } = props;
  const nodesById = new Map(definition.nodes.map((n) => [n.id, n]));
  const stats: NodeStats = new Map((report?.nodes ?? []).map((n) => [n.node_id, { entered: n.entered, stats: n.stats }]));
  const ctx: ChainCtx = { ...props, nodesById, stats };

  return (
    <PanZoom>
      <div className="flex flex-col items-center px-6 py-8">
        <button
          type="button"
          onClick={onSelectTrigger}
          className={cx(
            "w-[300px] rounded-xl border-2 bg-white px-4 py-3 text-left shadow-sm transition-colors",
            selected === "trigger" ? "border-accent-600 ring-4 ring-accent-100" : "border-accent-200 hover:border-accent-400",
          )}
        >
          <div className="flex items-center gap-2 text-[11px] font-semibold uppercase tracking-wider text-accent-700">
            <Zap className="size-3.5" /> Trigger
          </div>
          <div className="mt-1 text-sm font-medium text-zinc-900">{triggerLabel}</div>
          {report ? <div className="mt-1 text-xs text-zinc-500">{formatNumber(report.stats.recipients)} recipients so far</div> : null}
        </button>
        <Chain slot={{ kind: "entry" }} ctx={ctx} seen={new Set()} />
      </div>
    </PanZoom>
  );
}

type ChainCtx = CanvasProps & { nodesById: Map<string, FlowNode>; stats: NodeStats };

function Connector({ short }: { short?: boolean }) {
  return <div className={cx("w-px bg-zinc-300", short ? "h-3" : "h-5")} />;
}

function AddButton({ slot, ctx }: { slot: Slot; ctx: ChainCtx }) {
  if (ctx.readOnly) return <Connector />;
  return (
    <>
      <Connector short />
      <Menu
        align="left"
        trigger={(p) => (
          <button
            type="button"
            {...p}
            aria-label="Add step here"
            title="Add step"
            className="flex size-6 items-center justify-center rounded-full border border-zinc-300 bg-white text-zinc-500 shadow-xs transition-colors hover:border-accent-500 hover:bg-accent-50 hover:text-accent-700"
          >
            <Plus className="size-3.5" />
          </button>
        )}
        items={insertableTypes.map((t) => {
          const Icon = nodeIcons[t].icon;
          return { label: nodeLabels[t], icon: <Icon className="size-3.5 text-zinc-400" />, onSelect: () => ctx.onInsert(slot, t) };
        })}
      />
      <Connector short />
    </>
  );
}

function Chain({ slot, ctx, seen }: { slot: Slot; ctx: ChainCtx; seen: Set<string> }) {
  const id = getSlot(ctx.definition, slot);
  const node = id ? ctx.nodesById.get(id) : undefined;
  if (!node) {
    return (
      <>
        <AddButton slot={slot} ctx={ctx} />
        <div className="rounded-full bg-zinc-100 px-3 py-1 text-[11px] font-medium text-zinc-500">End</div>
      </>
    );
  }
  if (seen.has(node.id)) {
    return (
      <>
        <Connector />
        <button
          type="button"
          onClick={() => ctx.onSelectNode(node.id)}
          className="flex items-center gap-1.5 rounded-full border border-dashed border-zinc-300 bg-white px-3 py-1 text-xs text-zinc-600 hover:border-zinc-400"
        >
          <CornerDownRight className="size-3" /> Continues at {nodeTitle(node)}
        </button>
      </>
    );
  }
  const nextSeen = new Set(seen).add(node.id);
  return (
    <>
      <AddButton slot={slot} ctx={ctx} />
      <NodeCard node={node} ctx={ctx} />
      {isBranch(node) ? <Connector short /> : null}
      {isBranch(node) ? (
        <div className="grid grid-cols-[1fr_1fr]">
          {(["yes", "no"] as const).map((branch) => (
            <div key={branch} className="flex flex-col items-center">
              <div className="flex w-full">
                <div className={cx("h-5 w-1/2", branch === "no" && "border-t border-zinc-300")} />
                <div className={cx("h-5 w-1/2", branch === "yes" && "border-t border-zinc-300")} />
              </div>
              <div className="-mt-5 h-5 w-px bg-zinc-300" />
              <div className="flex flex-col items-center px-5">
              <span
                className={cx(
                  "rounded-full px-2 py-0.5 text-[10px] font-bold uppercase tracking-wider",
                  branch === "yes" ? "bg-accent-100 text-accent-800" : "bg-zinc-200 text-zinc-700",
                )}
              >
                {branchLabel(node, branch)}
              </span>
              <Chain slot={{ kind: branch, nodeId: node.id }} ctx={ctx} seen={nextSeen} />
              </div>
            </div>
          ))}
        </div>
      ) : (
        <Chain slot={{ kind: "next", nodeId: node.id }} ctx={ctx} seen={nextSeen} />
      )}
    </>
  );
}

/** "yes"/"no" for a conditional split; "A · 50%"/"B · 50%" for a random split; "replied"/"no reply" for a wait. */
function branchLabel(node: ConditionNode | RandomSplitNode | WaitForReplyNode, branch: "yes" | "no"): string {
  if (node.type === "condition") return branch;
  if (node.type === "wait_for_reply") return branch === "yes" ? "replied" : "no reply";
  return branch === "yes" ? `A · ${node.percent}%` : `B · ${100 - node.percent}%`;
}

function nodeTitle(node: FlowNode): string {
  if ((node.type === "email" || node.type === "sms" || node.type === "notify" || node.type === "review_request") && node.name) return node.name;
  return nodeLabels[node.type];
}

function nodeSummary(node: FlowNode, lists: List[]): { text: string; warning?: string } {
  const listName = (id: string) => lists.find((l) => l.id === id)?.name ?? "a list";
  switch (node.type) {
    case "delay":
      return { text: delaySummary(node) };
    case "email":
      return node.content.subject ? { text: node.content.subject } : { text: "No subject", warning: "Add a subject line" };
    case "sms":
      return node.content.body ? { text: node.content.body } : { text: "Empty message", warning: "Write a message" };
    case "condition": {
      const event = eventSplitSummary(node.event_condition);
      if (node.condition.conditions.length) return { text: [definitionSummary(node.condition, listName), event].filter(Boolean).join("; ") };
      return event ? { text: event } : { text: "No conditions", warning: "Add a condition" };
    }
    case "random_split":
      return { text: `${node.percent}% A / ${100 - node.percent}% B` };
    case "wait_for_reply":
      return { text: `Up to ${node.amount} ${node.amount === 1 ? node.unit.replace(/s$/, "") : node.unit} for them to text back` };
    case "update_profile": {
      const keys = Object.keys(node.properties);
      return keys.length ? { text: `Set ${keys.join(", ")}` } : { text: "No properties", warning: "Add a property" };
    }
    case "add_to_list":
    case "remove_from_list":
      return node.list_id ? { text: listName(node.list_id) } : { text: "No list", warning: "Choose a list" };
    case "webhook":
      return { text: `${node.method ?? "POST"} ${node.url}` };
    case "notify":
      return notifySummary(node);
    case "review_request":
      return reviewStepSummary(node);
  }
}

function NodeCard({ node, ctx }: { node: FlowNode; ctx: ChainCtx }) {
  const meta = nodeIcons[node.type];
  const Icon = meta.icon;
  const summary = nodeSummary(node, ctx.lists);
  const s = ctx.stats.get(node.id);
  const r = ctx.results?.find((x) => x.node_id === node.id);
  const isMessage = node.type === "email" || node.type === "sms" || node.type === "review_request";
  const selected = ctx.selected === node.id;
  return (
    <button
      type="button"
      onClick={() => ctx.onSelectNode(node.id)}
      className={cx(
        "w-[300px] rounded-xl border bg-white px-3.5 py-3 text-left shadow-xs transition-colors",
        selected ? "border-accent-600 ring-4 ring-accent-100" : "border-zinc-200 hover:border-zinc-400",
      )}
    >
      <div className="flex items-start gap-2.5">
        <span className={cx("flex size-7 shrink-0 items-center justify-center rounded-lg", meta.tone)}>
          <Icon className="size-3.5" />
        </span>
        <span className="min-w-0 flex-1">
          <span className="flex items-center gap-1.5">
            <span className="truncate text-sm font-medium text-zinc-900">{nodeTitle(node)}</span>
            {summary.warning ? <AlertTriangle className="size-3.5 shrink-0 text-amber-500" aria-label={summary.warning} /> : null}
          </span>
          <span className="line-clamp-2 text-xs text-zinc-500">{summary.text}</span>
        </span>
      </div>
      {r ? (
        <StepResultLine result={r} onWaiting={ctx.onWaiting ? () => ctx.onWaiting?.(node.id) : undefined} />
      ) : s ? (
        <div className="mt-2 flex flex-wrap gap-x-3 gap-y-0.5 border-t border-zinc-100 pt-2 text-[11px] tabular-nums text-zinc-600">
          <span>{formatNumber(s.entered)} entered</span>
          {isMessage ? (
            <>
              {node.type === "email" ? <span>{formatPercent(s.stats.open_rate)} open</span> : null}
              <span>{formatPercent(s.stats.click_rate)} click</span>
              <span className="font-medium text-zinc-800">{formatCurrency(s.stats.revenue)}</span>
            </>
          ) : null}
        </div>
      ) : null}
    </button>
  );
}
