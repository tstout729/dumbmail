import { UserPlus } from "lucide-react";
import { Badge, Button, InlineError, Modal, Notice, Skeleton } from "../../components/ui";
import type { Tone } from "../../components/ui";
import { formatDateTime, formatNumber } from "../../lib/format";
import { toast } from "../../lib/toast";
import { useBackfillDryRun, useBackfills, useCancelBackfill, useStartBackfill } from "./api";
import type { BackfillStatus } from "./types";

const tone: Record<BackfillStatus, Tone> = { running: "blue", done: "green", cancelled: "gray", failed: "red" };

const triggerText = {
  list: "everyone on the trigger list now",
  segment: "everyone in the trigger segment now",
  date_property: "",
  metric: "",
};

type Props = { flowId: string; live: boolean; canStart: boolean; timeZone: string; justTurnedOn?: boolean; onClose: () => void };

/** "Add people who already qualify": a count first, then a confirmed back-populate on the background lane. */
export function BackfillDialog({ flowId, live, canStart, timeZone, justTurnedOn, onClose }: Props) {
  const history = useBackfills(flowId, true);
  const running = history.data?.find((b) => b.status === "running") ?? null;
  const dry = useBackfillDryRun(flowId, !running);
  const start = useStartBackfill(flowId);
  const cancel = useCancelBackfill(flowId);
  const d = dry.data;
  return (
    <Modal
      title="Add people who already qualify"
      description={justTurnedOn ? "Your flow is live. New people start it from now on; you can also add the people who already qualify." : undefined}
      onClose={onClose}
      footer={
        <>
          <Button variant="secondary" onClick={onClose}>
            {running ? "Close" : "Not now"}
          </Button>
          {running ? (
            <Button variant="danger" loading={cancel.isPending} disabled={!canStart} onClick={() => cancel.mutate(running.id, { onSuccess: () => toast.success("Stopped adding people") })}>
              Stop adding
            </Button>
          ) : (
            <Button
              variant="primary"
              icon={<UserPlus className="size-3.5" />}
              disabled={!d || d.would_add === 0 || !live || !canStart}
              loading={start.isPending}
              onClick={() => start.mutate(undefined, { onSuccess: () => toast.success("Adding people in the background") })}
            >
              {d ? `Add ${formatNumber(d.would_add)} ${d.would_add === 1 ? "person" : "people"}` : "Add people"}
            </Button>
          )}
        </>
      }
    >
      <div className="space-y-4 text-sm">
        {running ? (
          <Notice tone="blue" title="Adding people now">
            {running.total === null
              ? "Working out who qualifies…"
              : `${formatNumber(running.added)} added, ${formatNumber(running.skipped)} skipped, of ${formatNumber(running.total)}.`}
          </Notice>
        ) : dry.isPending ? (
          <Skeleton className="h-16 w-full" />
        ) : dry.error ? (
          <InlineError message={dry.error.message} />
        ) : d ? (
          <>
            <p className="text-zinc-700">
              This adds {triggerText[d.trigger]}, following the flow's re-entry rules, profile filter and exit condition. Messages still check consent when they go out.
            </p>
            <div className="rounded-lg bg-zinc-50 p-3">
              <div className="text-[11px] text-zinc-500">Would add</div>
              <div className="text-2xl font-semibold tabular-nums">{formatNumber(d.would_add)}</div>
              {d.more_than_max ? <div className="text-xs text-amber-700">More qualify; one run adds at most {formatNumber(d.max_people)}.</div> : null}
              <div className="mt-1 text-xs text-zinc-500">{d.note}</div>
            </div>
            {!live ? <Notice tone="amber" title="Turn the flow on first">People can only be added to a live flow.</Notice> : null}
            {!canStart ? <Notice tone="gray" title="Ask a manager">Your role can see this count but can't add people.</Notice> : null}
          </>
        ) : null}
        {history.data && history.data.length > 0 ? (
          <div>
            <div className="mb-1 text-xs font-medium text-zinc-700">Earlier</div>
            <ul className="divide-y divide-zinc-100 text-xs">
              {history.data.slice(0, 5).map((b) => (
                <li key={b.id} className="flex items-center justify-between gap-2 py-1.5">
                  <span className="text-zinc-600">{formatDateTime(b.created_at, timeZone)}</span>
                  <span className="flex items-center gap-2 tabular-nums text-zinc-700">
                    {formatNumber(b.added)} added
                    <Badge tone={tone[b.status]}>{b.status}</Badge>
                  </span>
                </li>
              ))}
            </ul>
          </div>
        ) : null}
      </div>
    </Modal>
  );
}
