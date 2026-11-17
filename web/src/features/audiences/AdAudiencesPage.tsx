import { useState } from "react";
import { useSearchParams } from "react-router";
import { AlertTriangle, CheckCircle2, Megaphone, MoreHorizontal, Pause, Play, Plus, RefreshCw, Trash2, XCircle } from "lucide-react";
import { useSession } from "../../api";
import {
  Badge,
  Button,
  Card,
  Checkbox,
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
import { formatDate, formatNumber, formatRelative, pluralize } from "../../lib/format";
import { toast } from "../../lib/toast";
import { useAdConnections, useAudienceSyncs, useDisconnect, useRunSync, useSaveAdSettings, useStartConnect, useUpdateSync } from "./api";
import { NewSyncModal } from "./NewSyncModal";
import { SyncDrawer, syncState } from "./SyncDrawer";
import type { AdConnection, AdPlatform, AdSettings, AudienceSync } from "./types";

const callbackErrors: Record<string, string> = {
  cancelled: "The sign-in was cancelled, so nothing was connected.",
  expired: "That sign-in link expired. Start again.",
  signed_out: "You were signed out of DumbMail during the sign-in. Sign in and try again.",
  wrong_account: "That sign-in was started by a different person or DumbMail account. Start again from here.",
  failed: "The platform didn't finish the sign-in. Try again; if it keeps failing, check the app setup in docs/AD_AUDIENCES.md.",
};

const platformNames: Record<AdPlatform, string> = { meta: "Meta", google: "Google Ads", tiktok: "TikTok" };

const platformBlurb: Record<AdPlatform, string> = {
  meta: "Facebook and Instagram ads, from customer list audiences.",
  google: "Search, YouTube, Gmail and Display ads, with Customer Match.",
  tiktok: "TikTok ads, from customer file audiences (email only, at least 1,000 people).",
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
  const googleConnected = (connections.data?.data ?? []).some((c) => c.platform === "google" && c.status !== "not_connected");

  return (
    <div className="space-y-4">
      <p className="text-sm text-zinc-500">
        Send a list or segment to Meta, Google Ads or TikTok as an ad audience, and keep it current as people join and leave. Only people who are subscribed to
        your email, aren't suppressed and haven't opted out of ads are sent, as hashed email addresses (and phone numbers they opted in to texts with). Anyone
        who unsubscribes or opts out is removed on the next sync.
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

      {connections.data && (googleConnected || connections.data.settings.google_consent) ? <GoogleConsent settings={connections.data.settings} /> : null}

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
          settings={connections.data?.settings ?? null}
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

function GoogleConsent({ settings }: { settings: AdSettings }) {
  const { account } = useSession();
  const save = useSaveAdSettings();
  const [turningOff, setTurningOff] = useState(false);
  const set = (on: boolean) =>
    save.mutate({ google_consent: on }, { onSuccess: () => toast.success(on ? "Google consent setting turned on" : "Google consent setting turned off; Google syncs are paused") });
  return (
    <Card title="Google consent">
      <div className="space-y-2">
        <Checkbox
          label="The marketing consent we collect covers using customer data for advertising, as Google requires for users in the EEA, the UK and Switzerland."
          description="With this on, DumbMail sends everyone it uploads to Google with both of Google's consent signals granted. With it off, Google syncs can't be created and existing ones pause. Only the account's owner or an admin can change it."
          checked={settings.google_consent}
          disabled={save.isPending}
          onChange={(e) => (e.target.checked ? set(true) : setTurningOff(true))}
        />
        {settings.google_consent && settings.google_consent_at ? (
          <p className="pl-6 text-xs text-zinc-500">
            Turned on {formatDate(settings.google_consent_at, account.timezone)}
            {settings.google_consent_by ? ` by ${settings.google_consent_by}` : ""}.
          </p>
        ) : null}
      </div>
      {turningOff ? (
        <ConfirmDialog
          title="Turn off the Google consent setting?"
          body="Every Google sync pauses. Paused syncs add nobody, but still remove people who leave or opt out."
          confirmLabel="Turn off"
          danger
          loading={save.isPending}
          onConfirm={() => {
            set(false);
            setTurningOff(false);
          }}
          onClose={() => setTurningOff(false)}
        />
      ) : null}
    </Card>
  );
}

function ConnectionRow({ connection: c }: { connection: AdConnection }) {
  const { account } = useSession();
  const start = useStartConnect();
  const disconnect = useDisconnect();
  const [confirming, setConfirming] = useState(false);
  const expiresSoon = c.expires_soon;

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
          {c.available && !c.verified_live ? <Badge tone="amber">Not yet verified against a live account</Badge> : null}
        </div>
        <p className="mt-0.5 text-xs text-zinc-500">
          {c.status === "connected" && c.connected_as ? `Connected as ${c.connected_as}${c.syncs ? ` · ${pluralize(c.syncs, "sync")}` : ""}` : platformBlurb[c.platform]}
        </p>
        {expiresSoon && c.token_expires_at ? (
          <p className="mt-0.5 text-xs text-amber-700">The sign-in expires {formatDate(c.token_expires_at, account.timezone)}. Reconnect before then so syncs keep running.</p>
        ) : null}
        {c.status === "needs_reconnect" && c.error ? <p className="mt-0.5 text-xs text-amber-700">{c.error}</p> : null}
        {c.status === "not_connected" && c.last_revoke === "not_confirmed" ? (
          <p className="mt-0.5 text-xs text-amber-700">
            {c.label} didn't confirm it revoked DumbMail's access when you disconnected. Remove DumbMail from your {c.label} account's connected apps to be sure.
          </p>
        ) : null}
      </div>
      {c.available ? (
        <div className="flex items-center gap-2">
          {c.status !== "not_connected" ? (
            <Button size="sm" variant="ghost" onClick={() => setConfirming(true)}>
              Disconnect
            </Button>
          ) : null}
          {c.status !== "connected" || expiresSoon ? (
            <Button size="sm" variant={c.status === "needs_reconnect" || expiresSoon ? "primary" : "secondary"} loading={start.isPending} onClick={connect}>
              {c.status === "not_connected" ? "Connect" : "Reconnect"}
            </Button>
          ) : null}
        </div>
      ) : null}
      {confirming ? (
        <ConfirmDialog
          title={`Disconnect ${c.label}?`}
          body={`DumbMail deletes its sign-in and asks ${c.label} to revoke its access. Audiences already on ${c.label} stay as they are, and syncs wait until you connect again.`}
          confirmLabel="Disconnect"
          danger
          loading={disconnect.isPending}
          onConfirm={() =>
            disconnect.mutate(c.platform, {
              onSuccess: (r) => {
                setConfirming(false);
                if (r.revoked) toast.success(`${c.label} disconnected`);
                else toast.error(r.note);
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
          <Th className="text-right">Confirmed</Th>
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
  const unconfirmed = s.sent - s.confirmed;
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
        {formatNumber(s.confirmed)}
        {unconfirmed > 0 || s.pending_removal > 0 ? (
          <div className="text-xs text-zinc-500">
            {unconfirmed > 0 ? `${formatNumber(unconfirmed)} not confirmed` : ""}
            {unconfirmed > 0 && s.pending_removal > 0 ? " · " : ""}
            {s.pending_removal > 0 ? `${formatNumber(s.pending_removal)} pending removal` : ""}
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
              : { label: "Resume", icon: <Play className="size-3.5" />, disabled: s.pause_reason !== "user", onSelect: () => update.mutate({ status: "active" }) },
            { label: "Details and delete", icon: <Trash2 className="size-3.5" />, onSelect: onOpen },
          ]}
        />
      </Td>
    </tr>
  );
}
