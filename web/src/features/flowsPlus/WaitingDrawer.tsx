import { useState } from "react";
import { Users } from "lucide-react";
import { Link } from "react-router";
import { Button, Checkbox, ConfirmDialog, Drawer, EmptyState, InlineError, Skeleton } from "../../components/ui";
import { useCan } from "../team/api";
import { formatDateTime, formatPhone } from "../../lib/format";
import { toast } from "../../lib/toast";
import { useRemoveFromFlow, useWaiting } from "./api";

type Props = { flowId: string; nodeId: string; stepName: string; timeZone: string; onClose: () => void };

/** The people waiting at a step, with "remove from flow" for one or many. */
export function WaitingDrawer({ flowId, nodeId, stepName, timeZone, onClose }: Props) {
  const [pages, setPages] = useState<(string | null)[]>([null]);
  const after = pages[pages.length - 1];
  const page = useWaiting(flowId, nodeId, after);
  const [picked, setPicked] = useState<Set<string>>(new Set());
  const [confirming, setConfirming] = useState(false);
  const remove = useRemoveFromFlow(flowId);
  const canRemove = useCan("flows.live");
  const rows = page.data?.data ?? [];
  const toggle = (id: string) => setPicked((p) => {
    const n = new Set(p);
    if (n.has(id)) n.delete(id);
    else n.add(id);
    return n;
  });
  return (
    <Drawer
      title={`Waiting at ${stepName}`}
      onClose={onClose}
      width="w-[min(560px,100vw)]"
      footer={
        <div className="flex w-full flex-wrap items-center justify-between gap-2">
          <div className="flex gap-2">
            <Button size="sm" variant="secondary" disabled={pages.length < 2} onClick={() => setPages((p) => p.slice(0, -1))}>
              Previous
            </Button>
            <Button size="sm" variant="secondary" disabled={!page.data?.next_after} onClick={() => setPages((p) => [...p, page.data?.next_after ?? null])}>
              Next
            </Button>
          </div>
          {canRemove ? (
            <Button size="sm" variant="danger" disabled={picked.size === 0} onClick={() => setConfirming(true)}>
              Remove {picked.size > 0 ? picked.size : ""} from flow
            </Button>
          ) : null}
        </div>
      }
    >
      {page.isPending ? (
        <div className="space-y-2">
          {[0, 1, 2, 3, 4].map((i) => (
            <Skeleton key={i} className="h-10 w-full" />
          ))}
        </div>
      ) : page.error ? (
        <InlineError message={page.error.message} />
      ) : rows.length === 0 ? (
        <EmptyState icon={<Users className="size-5" />} title="Nobody is waiting here" description="People show up here while they wait at this step." />
      ) : (
        <ul className="divide-y divide-zinc-100">
          {rows.map((r) => {
            const name = [r.first_name, r.last_name].filter(Boolean).join(" ");
            return (
              <li key={r.run_id} className="flex items-start gap-3 py-2.5">
                {canRemove ? <Checkbox label="" aria-label={`Pick ${r.email ?? r.phone_number ?? "person"}`} checked={picked.has(r.run_id)} onChange={() => toggle(r.run_id)} className="mt-0.5" /> : null}
                <div className="min-w-0 flex-1">
                  <Link to={`/audience/profiles/${r.profile_id}`} className="block truncate text-sm font-medium text-zinc-900 hover:text-accent-700">
                    {name || r.email || formatPhone(r.phone_number) || "No name"}
                  </Link>
                  <div className="truncate text-xs text-zinc-500">{name ? r.email ?? formatPhone(r.phone_number) : null}</div>
                </div>
                <div className="shrink-0 text-right text-xs text-zinc-500">
                  <div>Entered {formatDateTime(r.entered_at, timeZone)}</div>
                  {r.waiting_until ? <div>Until {formatDateTime(r.waiting_until, timeZone)}</div> : null}
                </div>
              </li>
            );
          })}
        </ul>
      )}
      {confirming ? (
        <ConfirmDialog
          title={`Remove ${picked.size} ${picked.size === 1 ? "person" : "people"} from this flow?`}
          body="They get nothing more from this flow. They can enter it again only as its re-entry rules allow."
          confirmLabel="Remove from flow"
          danger
          loading={remove.isPending}
          onConfirm={() =>
            remove.mutate([...picked], {
              onSuccess: (r) => {
                toast.success(`Removed ${r.removed} from the flow`);
                setPicked(new Set());
                setConfirming(false);
              },
            })
          }
          onClose={() => setConfirming(false)}
        />
      ) : null}
    </Drawer>
  );
}
