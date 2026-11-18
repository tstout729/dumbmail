import { useState } from "react";
import { Inbox, RotateCcw } from "lucide-react";
import { errorMessage } from "../../api/client";
import { useSession } from "../../api";
import { Button, ConfirmDialog, Drawer, EmptyRow, EmptyState, InlineError, Segmented, Skeleton, SkeletonRows, Table, Td, Th } from "../../components/ui";
import { formatDateTime, formatRelative } from "../../lib/format";
import { toast } from "../../lib/toast";
import { useDeliveries, useDelivery, useReplay } from "./api";
import { DeliveryStatusBadge, JsonBlock, countsSummary } from "./parts";
import type { DeliveryStatus, Integration } from "./types";

const sources = { webhook: "Webhook", pull: "Pull", fetch: "Fetch", test: "Test", replay: "Replay" } as const;

export function DeliveriesTab({ integration: i, canManage }: { integration: Integration; canManage: boolean }) {
  const { account } = useSession();
  const [status, setStatus] = useState<DeliveryStatus | "">("");
  const list = useDeliveries(i.id, status);
  const replay = useReplay(i.id);
  const [open, setOpen] = useState<string | null>(null);
  const [replayAll, setReplayAll] = useState(false);
  const rows = list.data?.data ?? [];
  const doReplay = (input: { delivery_ids?: string[]; all_failed?: boolean }) =>
    replay.mutate(input, {
      onSuccess: (r) => {
        setReplayAll(false);
        toast.success(r.replayed === 0 ? "Nothing to replay" : `Replaying ${r.replayed} ${r.replayed === 1 ? "delivery" : "deliveries"}`);
        void list.refetch();
      },
      onError: (e) => {
        setReplayAll(false);
        toast.error(errorMessage(e));
      },
    });

  return (
    <div className="min-w-0 space-y-3">
      <div className="flex flex-wrap items-center justify-between gap-2">
        <Segmented
          value={status === "" ? "all" : status}
          onChange={(v) => setStatus(v === "all" ? "" : v)}
          size="sm"
          options={[
            { value: "all", label: "All" },
            { value: "error", label: "Failed" },
            { value: "ok", label: "Done" },
            { value: "ignored", label: "Nothing to do" },
            { value: "pending", label: "Waiting" },
          ]}
        />
        {canManage ? (
          <Button size="sm" variant="secondary" icon={<RotateCcw className="size-3.5" />} onClick={() => setReplayAll(true)}>
            Replay all failed
          </Button>
        ) : null}
      </div>
      <p className="text-xs text-zinc-500">Kept for 30 days. Failed webhooks retry after 1, 5, 30 and 120 minutes, then wait for a replay.</p>
      <Table>
        <thead>
          <tr>
            <Th>Received</Th>
            <Th>From</Th>
            <Th>Status</Th>
            <Th>Result</Th>
            <Th className="w-24" />
          </tr>
        </thead>
        <tbody>
          {list.isPending ? (
            <SkeletonRows cols={5} />
          ) : list.isError ? (
            <EmptyRow cols={5}>
              <div className="py-6">
                <InlineError message={errorMessage(list.error)} />
              </div>
            </EmptyRow>
          ) : rows.length === 0 ? (
            <EmptyRow cols={5}>
              <EmptyState icon={<Inbox className="size-5" />} title={status ? "Nothing here" : "Nothing received yet"} description={status ? "No deliveries with this status in the last 30 days." : "Webhooks and pulls show up here as they arrive, with what was written."} />
            </EmptyRow>
          ) : (
            rows.map((d) => (
              <tr key={d.id} className="cursor-pointer hover:bg-zinc-50" onClick={() => setOpen(d.id)}>
                <Td>
                  <div className="whitespace-nowrap text-zinc-900">{formatRelative(d.created_at)}</div>
                  <div className="whitespace-nowrap text-xs text-zinc-500">{formatDateTime(d.created_at, account.timezone)}</div>
                </Td>
                <Td className="text-zinc-600">{sources[d.source]}</Td>
                <Td>
                  <DeliveryStatusBadge status={d.status} retrying={d.status === "error" && d.next_attempt_at !== null} />
                </Td>
                <Td className="max-w-[340px]">
                  <div className="truncate text-sm text-zinc-700">{d.error ?? (countsSummary(d.counts) || "—")}</div>
                </Td>
                <Td className="text-right">
                  {canManage && d.status === "error" && d.replayable ? (
                    <Button
                      size="sm"
                      variant="ghost"
                      onClick={(e) => {
                        e.stopPropagation();
                        doReplay({ delivery_ids: [d.id] });
                      }}
                    >
                      Replay
                    </Button>
                  ) : null}
                </Td>
              </tr>
            ))
          )}
        </tbody>
      </Table>
      {list.data && list.data.total > rows.length ? <p className="text-xs text-zinc-500">Showing the newest {rows.length} of {list.data.total}.</p> : null}
      {open ? <DeliveryDrawer integrationId={i.id} deliveryId={open} onClose={() => setOpen(null)} onReplay={(id) => doReplay({ delivery_ids: [id] })} /> : null}
      {replayAll ? (
        <ConfirmDialog
          title="Replay every failed delivery?"
          body="Each one runs again with the current script (up to 500). It writes profiles and events, and flows triggered by those events can send messages."
          confirmLabel="Replay"
          loading={replay.isPending}
          onClose={() => setReplayAll(false)}
          onConfirm={() => doReplay({ all_failed: true })}
        />
      ) : null}
    </div>
  );
}

function DeliveryDrawer({ integrationId, deliveryId, onClose, onReplay }: { integrationId: string; deliveryId: string; onClose: () => void; onReplay: (id: string) => void }) {
  const { account } = useSession();
  const q = useDelivery(integrationId, deliveryId);
  const d = q.data;
  return (
    <Drawer
      title="Delivery"
      onClose={onClose}
      width="w-[min(640px,100vw)]"
      footer={
        d && d.body ? (
          <Button variant="secondary" icon={<RotateCcw className="size-3.5" />} onClick={() => onReplay(d.id)}>
            Replay with the current script
          </Button>
        ) : null
      }
    >
      {q.isPending ? (
        <Skeleton className="h-40 w-full" />
      ) : q.isError ? (
        <InlineError message={errorMessage(q.error)} />
      ) : d ? (
        <div className="space-y-4 text-sm">
          <div className="flex flex-wrap items-center gap-2">
            <DeliveryStatusBadge status={d.status} retrying={d.status === "error" && d.next_attempt_at !== null} />
            <span className="text-zinc-600">
              {sources[d.source]} · {formatDateTime(d.created_at, account.timezone)}
              {d.duration_ms !== null ? ` · ${d.duration_ms} ms` : ""} · {d.attempts} {d.attempts === 1 ? "try" : "tries"}
            </span>
          </div>
          {d.next_attempt_at ? <p className="text-xs text-amber-700">Next try {formatRelative(d.next_attempt_at)}.</p> : null}
          {d.error ? <div className="break-words rounded-md border border-red-200 bg-red-50 px-3 py-2 text-red-800">{d.error}</div> : null}
          {d.payload_hidden ? (
            <p className="text-xs text-zinc-500">
              {d.counts ? `${countsSummary(d.counts) || "Nothing written"}. ` : ""}Payloads and what the script wrote can hold people's details, so only owners and admins see them.
            </p>
          ) : null}
          {d.result ? (
            <section>
              <h3 className="mb-1 text-xs font-medium text-zinc-700">What was written</h3>
              <p className="mb-2 text-zinc-700">{countsSummary(d.result.counts) || "Nothing"}</p>
              {d.result.effects.length > 0 ? (
                <ul className="space-y-1">
                  {d.result.effects.map((e, n) => (
                    <li key={n} className="break-words rounded bg-zinc-50 px-2 py-1 font-mono text-xs text-zinc-700">
                      {e}
                    </li>
                  ))}
                </ul>
              ) : null}
              {d.result.logs.length > 0 ? <JsonBlock value={d.result.logs.join("\n")} className="mt-2" /> : null}
            </section>
          ) : null}
          {d.payload_hidden ? null : (
            <section>
              <h3 className="mb-1 text-xs font-medium text-zinc-700">Payload</h3>
              {d.body ? (
                <JsonBlock value={d.payload} />
              ) : (
                <p className="text-xs text-zinc-500">Not kept (the signature check failed, the integration was off, the answer was over 1 MB, or the account's payload storage was full).</p>
              )}
            </section>
          )}
          {d.request ? (
            <section>
              <h3 className="mb-1 text-xs font-medium text-zinc-700">Request (secrets hidden)</h3>
              <JsonBlock value={d.request} />
            </section>
          ) : null}
          {d.headers && Object.keys(d.headers).length > 0 ? (
            <details>
              <summary className="cursor-pointer text-xs font-medium text-zinc-700">Headers</summary>
              <JsonBlock value={d.headers} className="mt-1" />
            </details>
          ) : null}
        </div>
      ) : null}
    </Drawer>
  );
}
