import { useState } from "react";
import { useSearchParams } from "react-router";
import { AlertTriangle, CheckCircle2, Megaphone, MoreHorizontal, Pause, Play, Plus, RefreshCw, Trash2, XCircle } from "lucide-react";
import {
  Badge,
  Button,
  Card,
  ConfirmDialog,
  EmptyRow,
  EmptyState,
  IconButton,
  Menu,
  Notice,
  Skeleton,
  SkeletonRows,
  Table,
  Td,
  Th,
} from "../../components/ui";
import { formatNumber, formatRelative, pluralize } from "../../lib/format";
import { toast } from "../../lib/toast";
import { useAdConnections, useAudienceSyncs, useDisconnect, useRunSync, useStartConnect, useUpdateSync } from "./api";
import { NewSyncModal } from "./NewSyncModal";
import { SyncDrawer, syncState } from "./SyncDrawer";
import type { AdConnection, AdPlatform, AudienceSync } from "./types";

const callbackErrors: Record<string, string> = {
  cancelled: "The sign-in was cancelled, so nothing was connected.",
  expired: "That sign-in link expired. Start again.",
  signed_out: "You were signed out of DumbMail during the sign-in. Sign in and try again.",
  wrong_account: "That sign-in was started from a different DumbMail account or person. Start again from here.",
  failed: "The platform didn't finish the sign-in. Try again; if it keeps failing, check the app setup in docs/AD_AUDIENCES.md.",
};

const platformNames: Record<AdPlatform, string> = { meta: "Meta", google: "Google Ads", tiktok: "TikTok" };

const platformBlurb: Record<AdPlatform, string> = {
  meta: "Facebook and Instagram ads, from customer list audiences.",
  google: "Search, YouTube, Gmail and Display ads, with Customer Match.",
  tiktok: "TikTok ads, from customer file audiences.",
};

export function AdAudiencesPage() {
  const [params, setParams] = useSearchParams();
  const [newSync, setNewSync] = useState<string | null | false>(() => params.get("new") ?? false);
  const [openId, setOpenId] = useState<string | null>(null);
  const connections = useAdConnections();
  const syncs = useAudienceSyncs();

  const connected = params.get("connected");
  const error = params.get("error");
  const clearParams = () => setParams({}, { replace: true });
  const anyConnected = (connections.data?.data ?? []).some((c) => c.status === "connected");

  return (
    <div className="space-y-4">
      <p className="text-sm text-zinc-500">
        Send a list or segment to Meta, Google Ads or TikTok as an ad audience, and keep it current as people join and leave. Contact details are hashed before
        they leave DumbMail. People who unsubscribed or opted out of ads are never sent.
      </p>
      {connected && connected in platformNames ? (
        <Notice tone="green" icon={<CheckCircle2 />} title={`${platformNames[connected as AdPlatform]} is connected`}>
          Now create a sync below.{" "}
          <button type="button" className="font-medium underline" onClick={clearParams}>
            Dismiss
          </button>
        </Notice>
      ) : null}
      {error ? (
        <Notice tone="red" icon={<XCircle />} title="Couldn't connect">
          {callbackErrors[error] ?? callbackErrors.failed}{" "}
          <button type="button" className="font-medium underline" onClick={clearParams}>
            Dismiss
          </button>
        </Notice>
      ) : null}

      <Card title="Ad platforms" padded={false}>
        <div className="divide-y divide-zinc-100">
          {connections.isPending
            ? [0, 1, 2].map((i) => (
                <div key={i} className="px-4 py-3">
                  <Skeleton className="h-5 w-48" />
                </div>
              ))
            : (connections.data?.data ?? []).map((c) => <ConnectionRow key={c.platform} connection={c} />)}
          {connections.error ? <p className="px-4 py-3 text-sm text-red-600">{connections.error.message}</p> : null}
        </div>
      </Card>

      <Card
        title="Synced audiences"
        padded={false}
        actions={
          <Button size="sm" variant="primary" icon={<Plus className="size-3.5" />} disabled={!anyConnected} onClick={() => setNewSync(null)}>
            New sync
          </Button>
        }
      >
        <SyncTable syncs={syncs.data} loading={syncs.isPending} error={syncs.error?.message} anyConnected={anyConnected} onOpen={setOpenId} onNew={() => setNewSync(null)} />
      </Card>

      {newSync !== false ? (
        <NewSyncModal
          connections={connections.data?.data ?? []}
          initialSource={newSync}
          onClose={() => {
            setNewSync(false);
            if (params.get("new")) clearParams();
          }}
          onCreated={(id) => {
            setNewSync(false);
            if (params.get("new")) clearParams();
            setOpenId(id);
          }}
        />
      ) : null}
      {openId ? <SyncDrawer id={openId} onClose={() => setOpenId(null)} /> : null}
    </div>
  );
}

function ConnectionRow({ connection: c }: { connection: AdConnection }) {
  const start = useStartConnect();
  const disconnect = useDisconnect();
  const [confirming, setConfirming] = useState(false);

  const connect = () =>
    start.mutate(c.platform, {
      onSuccess: ({ url }) => {
        window.location.assign(url);
      },
    });

  return (
    <div className="flex flex-wrap items-center justify-between gap-3 px-4 py-3">
      <div className="min-w-0">
        <div className="flex flex-wrap items-center gap-2">
          <span className="text-sm font-medium text-zinc-900">{c.label}</span>
          {!c.available ? (
            <Badge>Not available on this server</Badge>
          ) : c.status === "connected" ? (
            <Badge tone="green" dot>
              Connected
            </Badge>
          ) : c.status === "needs_reconnect" ? (
            <Badge tone="amber" dot>
              Reconnect needed
            </Badge>
          ) : null}
        </div>
        <p className="mt-0.5 text-xs text-zinc-500">
          {c.status === "connected" && c.connected_as ? `Connected as ${c.connected_as}${c.syncs ? ` · ${pluralize(c.syncs, "sync")}` : ""}` : platformBlurb[c.platform]}
        </p>
        {c.status === "needs_reconnect" && c.error ? <p className="mt-0.5 text-xs text-amber-700">{c.error}</p> : null}
      </div>
      {c.available ? (
        <div className="flex items-center gap-2">
          {c.status !== "not_connected" ? (
            <Button size="sm" variant="ghost" onClick={() => setConfirming(true)}>
              Disconnect
            </Button>
          ) : null}
          {c.status !== "connected" ? (
            <Button size="sm" variant={c.status === "needs_reconnect" ? "primary" : "secondary"} loading={start.isPending} onClick={connect}>
              {c.status === "needs_reconnect" ? "Reconnect" : "Connect"}
            </Button>
          ) : null}
        </div>
      ) : null}
      {confirming ? (
        <ConfirmDialog
          title={`Disconnect ${c.label}?`}
          body={`DumbMail's access is revoked and its sign-in deleted. Audiences already on ${c.label} stay as they are, and syncs wait until you connect again.`}
          confirmLabel="Disconnect"
          danger
          loading={disconnect.isPending}
          onConfirm={() =>
            disconnect.mutate(c.platform, {
              onSuccess: () => {
                setConfirming(false);
                toast.success(`${c.label} disconnected`);
              },
            })
          }
          onClose={() => setConfirming(false)}
        />
      ) : null}
    </div>
  );
}

type SyncTableProps = {
  syncs: AudienceSync[] | undefined;
  loading: boolean;
  error: string | undefined;
  anyConnected: boolean;
  onOpen: (id: string) => void;
  onNew: () => void;
};

function SyncTable({ syncs, loading, error, anyConnected, onOpen, onNew }: SyncTableProps) {
  return (
    <Table className="rounded-none border-0 shadow-none">
      <thead>
        <tr>
          <Th>Audience</Th>
          <Th>Platform</Th>
          <Th>Status</Th>
          <Th className="text-right">People sent</Th>
          <Th>Last sync</Th>
          <Th className="w-10" />
        </tr>
      </thead>
      <tbody>
        {loading ? (
          <SkeletonRows cols={6} rows={2} />
        ) : error ? (
          <EmptyRow cols={6}>
            <p className="px-4 py-6 text-sm text-red-600">{error}</p>
          </EmptyRow>
        ) : (syncs ?? []).length === 0 ? (
          <EmptyRow cols={6}>
            <EmptyState
              icon={<Megaphone className="size-5" />}
              title="No synced audiences yet"
              description={anyConnected ? "Pick a list or segment to send to an ad platform." : "Connect an ad platform above, then pick a list or segment to sync."}
              action={
                anyConnected ? (
                  <Button variant="primary" icon={<Plus className="size-3.5" />} onClick={onNew}>
                    New sync
                  </Button>
                ) : undefined
              }
            />
          </EmptyRow>
        ) : (
          (syncs ?? []).map((s) => <SyncRow key={s.id} sync={s} onOpen={() => onOpen(s.id)} />)
        )}
      </tbody>
    </Table>
  );
}

function SyncRow({ sync: s, onOpen }: { sync: AudienceSync; onOpen: () => void }) {
  const run = useRunSync();
  const update = useUpdateSync(s.id);
  const state = syncState(s);
  return (
    <tr
      tabIndex={0}
      onClick={onOpen}
      onKeyDown={(e) => {
        if (e.key === "Enter") onOpen();
      }}
      className="cursor-pointer transition-colors hover:bg-zinc-50 focus:bg-zinc-50 focus:outline-none [&:last-child>td]:border-b-0"
    >
      <Td>
        <div className="font-medium text-zinc-900">{s.name}</div>
        <div className="text-xs text-zinc-500">
          {s.source.type === "list" ? "List" : "Segment"}: {s.source.name ?? "deleted"}
          {s.include_unsubscribed ? " · includes unsubscribed" : ""}
        </div>
      </Td>
      <Td>
        <div className="text-zinc-800">{s.platform_label}</div>
        <div className="text-xs text-zinc-500">{s.ad_account_name ?? s.ad_account_id}</div>
      </Td>
      <Td>
        <Badge tone={state.tone} dot>
          {state.label}
        </Badge>
        {s.error ? (
          <div className="mt-1 flex max-w-xs items-start gap-1 text-xs text-red-700">
            <AlertTriangle className="mt-0.5 size-3 shrink-0" />
            <span className="line-clamp-2">{s.error}</span>
          </div>
        ) : null}
      </Td>
      <Td className="text-right tabular-nums">
        {formatNumber(s.members_sent)}
        {s.last_run_at && (s.last_added || s.last_removed) ? (
          <div className="text-xs text-zinc-500">
            +{formatNumber(s.last_added)} / −{formatNumber(s.last_removed)}
          </div>
        ) : null}
      </Td>
      <Td className="text-zinc-600">{s.running ? "Running now" : formatRelative(s.last_run_at)}</Td>
      <Td onClick={(e) => e.stopPropagation()}>
        <Menu
          trigger={(props) => (
            <IconButton label={`Actions for ${s.name}`} {...props}>
              <MoreHorizontal className="size-4" />
            </IconButton>
          )}
          items={[
            {
              label: "Sync now",
              icon: <RefreshCw className="size-3.5" />,
              disabled: s.running || s.status === "paused" || s.connection_status !== "connected",
              onSelect: () => run.mutate(s.id, { onSuccess: () => toast.success("Sync started") }),
            },
            s.status === "active"
              ? { label: "Pause", icon: <Pause className="size-3.5" />, onSelect: () => update.mutate({ status: "paused" }) }
              : { label: "Resume", icon: <Play className="size-3.5" />, onSelect: () => update.mutate({ status: "active" }) },
            { label: "Details and delete", icon: <Trash2 className="size-3.5" />, onSelect: onOpen },
          ]}
        />
      </Td>
    </tr>
  );
}
