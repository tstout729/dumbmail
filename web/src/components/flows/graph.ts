import type { ConditionNode, FlowDefinition, FlowNode, FlowNodeType, FlowTrigger, List, RandomSplitNode, Segment, WaitForReplyNode } from "../../api";
import { shortId } from "../../lib/id";
import { starterEmail } from "../email/blocks";
import { newReviewRequestNode } from "../../features/reviews/StepEditor";

/** A pointer in the graph that can hold a node id: the entry, a node's `next`, or a split's yes/no. */
export type Slot = { kind: "entry" } | { kind: "next" | "yes" | "no"; nodeId: string };

/** Nodes with two paths (yes/no) instead of a single `next`: conditional and random splits, and waiting for a reply. */
export function isBranch(node: FlowNode): node is ConditionNode | RandomSplitNode | WaitForReplyNode {
  return node.type === "condition" || node.type === "random_split" || node.type === "wait_for_reply";
}

export function getSlot(def: FlowDefinition, slot: Slot): string | null {
  if (slot.kind === "entry") return def.entry;
  const node = def.nodes.find((n) => n.id === slot.nodeId);
  if (!node) return null;
  if (isBranch(node)) return slot.kind === "yes" ? node.yes : slot.kind === "no" ? node.no : null;
  return slot.kind === "next" ? node.next : null;
}

function setPointer(node: FlowNode, kind: "next" | "yes" | "no", id: string | null): FlowNode {
  if (isBranch(node)) return kind === "no" ? { ...node, no: id } : { ...node, yes: id };
  return { ...node, next: id };
}

export function setSlot(def: FlowDefinition, slot: Slot, id: string | null): FlowDefinition {
  if (slot.kind === "entry") return { ...def, entry: id };
  return { ...def, nodes: def.nodes.map((n) => (n.id === slot.nodeId ? setPointer(n, slot.kind, id) : n)) };
}

export function newNode(type: FlowNodeType, brandColor?: string, firstListId?: string): FlowNode {
  const id = shortId();
  switch (type) {
    case "delay":
      return { id, type, amount: 1, unit: "days", next: null };
    case "email":
      return { id, type, name: "Email", content: { ...starterEmail(brandColor), subject: "" }, next: null };
    case "sms":
      return { id, type, name: "SMS", content: { body: "" }, next: null };
    case "condition":
      return { id, type, condition: { match: "all", conditions: [] }, yes: null, no: null };
    case "random_split":
      return { id, type, percent: 50, yes: null, no: null };
    case "wait_for_reply":
      return { id, type, amount: 1, unit: "days", yes: null, no: null };
    case "update_profile":
      return { id, type, properties: {}, next: null };
    case "add_to_list":
    case "remove_from_list":
      return { id, type, list_id: firstListId ?? "", next: null };
    case "webhook":
      return { id, type, url: "https://", method: "POST", next: null };
    case "notify":
      return { id, type, to: [], subject: "", body: "", next: null };
    case "review_request":
      return newReviewRequestNode(id, brandColor);
  }
}

/** Insert `node` at `slot`; whatever the slot pointed to now follows the new node (yes-branch for splits). */
export function insertAt(def: FlowDefinition, slot: Slot, node: FlowNode): FlowDefinition {
  const following = getSlot(def, slot);
  const linked = setPointer(node, isBranch(node) ? "yes" : "next", following);
  return setSlot({ ...def, nodes: [...def.nodes, linked] }, slot, node.id);
}

function successors(n: FlowNode): string[] {
  return (isBranch(n) ? [n.yes, n.no] : [n.next]).filter((x): x is string => x !== null);
}

/** Drop nodes no longer reachable from the entry. */
export function prune(def: FlowDefinition): FlowDefinition {
  const byId = new Map(def.nodes.map((n) => [n.id, n]));
  const seen = new Set<string>();
  const stack = def.entry ? [def.entry] : [];
  while (stack.length) {
    const id = stack.pop() as string;
    if (seen.has(id)) continue;
    const n = byId.get(id);
    if (!n) continue;
    seen.add(id);
    stack.push(...successors(n));
  }
  return { ...def, nodes: def.nodes.filter((n) => seen.has(n.id)) };
}

/** Remove a node, reconnecting its parents to what came after it (the Yes / A branch for splits). */
export function removeNode(def: FlowDefinition, nodeId: string): FlowDefinition {
  const node = def.nodes.find((n) => n.id === nodeId);
  if (!node) return def;
  const after = isBranch(node) ? node.yes : node.next;
  const relink = (id: string | null) => (id === nodeId ? after : id);
  const nodes = def.nodes
    .filter((n) => n.id !== nodeId)
    .map((n) => (isBranch(n) ? { ...n, yes: relink(n.yes), no: relink(n.no) } : { ...n, next: relink(n.next) }));
  return prune({ ...def, entry: relink(def.entry), nodes });
}

export function replaceNode(def: FlowDefinition, node: FlowNode): FlowDefinition {
  return { ...def, nodes: def.nodes.map((n) => (n.id === node.id ? node : n)) };
}

export function countDescendants(def: FlowDefinition, start: string | null): number {
  return start ? prune({ ...def, entry: start }).nodes.length : 0;
}

export const nodeLabels: Record<FlowNodeType, string> = {
  delay: "Time delay",
  email: "Email",
  sms: "SMS",
  condition: "Conditional split",
  random_split: "Random split",
  wait_for_reply: "Wait for a reply",
  update_profile: "Update profile",
  add_to_list: "Add to list",
  remove_from_list: "Remove from list",
  webhook: "Webhook",
  notify: "Notify the team",
  review_request: "Ask for a review",
};

export const insertableTypes: FlowNodeType[] = ["delay", "email", "sms", "condition", "random_split", "wait_for_reply", "update_profile", "add_to_list", "remove_from_list", "webhook", "notify", "review_request"];

export function triggerSummary(t: FlowTrigger, lists: List[] = [], segments: Segment[] = []): string {
  switch (t.type) {
    case "metric":
      return t.metric ? `When someone ${t.metric}` : "Metric not chosen";
    case "list":
      return `Added to ${lists.find((l) => l.id === t.list_id)?.name ?? "a list"}`;
    case "segment":
      return `Enters ${segments.find((s) => s.id === t.segment_id)?.name ?? "a segment"}`;
    case "date_property":
      return `On ${t.field || "a date property"}${t.offset_days ? ` ${t.offset_days > 0 ? "+" : ""}${t.offset_days}d` : ""}`;
    case "event_date": {
      const n = t.offset_days ?? 0;
      const when = n === 0 ? "On" : `${Math.abs(n)} ${Math.abs(n) === 1 ? "day" : "days"} ${n < 0 ? "before" : "after"}`;
      return `${when} ${t.field || "a date"} in ${t.metric || "an event"}`;
    }
  }
}

export function triggerKind(t: FlowTrigger): string {
  return { metric: "Metric", list: "List", segment: "Segment", date_property: "Date", event_date: "Event date" }[t.type];
}
