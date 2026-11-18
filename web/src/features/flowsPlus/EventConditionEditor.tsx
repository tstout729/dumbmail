import { Plus, ShieldAlert, X } from "lucide-react";
import type { ConditionNode, FlowTrigger } from "../../api/types";
import { FilterFields } from "../../components/segments/ConditionBuilder";
import { Button, IconButton, Notice, Segmented } from "../../components/ui";
import { useMetricTrust } from "./api";

type Props = { node: ConditionNode; onChange: (n: ConditionNode) => void; trigger?: FlowTrigger };

/** Filters on the event that started the flow (a trigger split), next to the profile conditions. */
export function EventConditionEditor({ node, onChange, trigger }: Props) {
  const metric = trigger?.type === "metric" ? trigger.metric : null;
  const ec = node.event_condition ?? { match: "all" as const, filters: [] };
  const trust = useMetricTrust(ec.filters.length > 0 ? metric : null);
  const set = (next: typeof ec) => {
    const out: ConditionNode = { ...node, event_condition: next };
    if (next.filters.length === 0) delete out.event_condition;
    onChange(out);
  };
  if (trigger?.type !== "metric") {
    return <p className="text-xs text-zinc-500">This flow doesn't start from an event, so there's no event to split on.</p>;
  }
  return (
    <div className="space-y-2 border-t border-zinc-100 pt-3">
      <div className="flex items-center justify-between gap-2">
        <div>
          <div className="text-xs font-medium text-zinc-700">The event that started the flow</div>
          <div className="text-xs text-zinc-500">Use properties.name, value or from_browser. Both the profile and the event must match for Yes.</div>
        </div>
        {ec.filters.length > 1 ? (
          <Segmented
            size="sm"
            value={ec.match}
            onChange={(match) => set({ ...ec, match })}
            options={[
              { value: "all", label: "All" },
              { value: "any", label: "Any" },
            ]}
          />
        ) : null}
      </div>
      {ec.filters.map((f, i) => (
        <div key={i} className="flex flex-wrap items-center gap-1.5 rounded-md border border-zinc-200 p-2">
          <FilterFields filter={f} fieldList={undefined} fieldPlaceholder="properties.nights" onChange={(nf) => set({ ...ec, filters: ec.filters.map((x, j) => (j === i ? nf : x)) })} />
          <IconButton label="Remove filter" size="sm" onClick={() => set({ ...ec, filters: ec.filters.filter((_, j) => j !== i) })}>
            <X className="size-3" />
          </IconButton>
        </div>
      ))}
      <Button size="sm" variant="ghost" icon={<Plus className="size-3" />} onClick={() => set({ ...ec, filters: [...ec.filters, { field: "properties.", op: "eq", value: "" }] })}>
        Add event filter
      </Button>
      {trust.data?.browser_can_send ? (
        <Notice tone="amber" icon={<ShieldAlert />} title="Your website can send this event">
          Anyone can send “{metric}” from a browser with made-up values. To split on trusted events only, add a filter: from_browser equals false.
        </Notice>
      ) : null}
    </div>
  );
}
