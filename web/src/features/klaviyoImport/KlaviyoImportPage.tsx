import { useState, type FormEvent } from "react";
import { Link } from "react-router";
import { AlertTriangle, CheckCircle2, CircleDashed, Download, KeyRound, Loader2, Pause, Play, RefreshCw, Search, XCircle } from "lucide-react";
import { useSession } from "../../api";
import { Badge, Button, Card, Checkbox, ConfirmDialog, Field, InlineError, Input, Notice, Select, Skeleton, StatusBadge, Table, Td, Th, Toggle, cx } from "../../components/ui";
import { formatCurrency, formatDate, formatDateTime, formatNumber, formatPercent, formatRelative } from "../../lib/format";
import {
  useCancelKlaviyo,
  useCheckSiteKlaviyo,
  useConnectKlaviyo,
  useKlaviyo,
  useKlaviyoCampaigns,
  useKlaviyoIssues,
  useKlaviyoReport,
  useKlaviyoSync,
  useKlaviyoSyncNow,
  useMarkKlaviyo,
  usePauseKlaviyo,
  useStartKlaviyo,
} from "./api";
import type { ChecklistItem, KlaviyoImport, KlaviyoState, KlaviyoSurvey, ReportLine, SurveyCount } from "./types";

const stepLabels: Record<string, string> = {
  metrics: "Metrics",
  profiles: "Profiles and consent",
  lists: "Lists",
  list_members: "List members",
  segments: "Segments",
  segment_members: "Segment snapshots",
  templates: "Templates",
  campaigns: "Campaigns",
  campaign_stats: "Campaign results",
  flows: "Flows",
  forms: "Forms",
  events: "Events",
  compare: "Klaviyo's counts for the report",
  sync_profiles: "Changed profiles",
  sync_list_members: "New list members",
  sync_events: "New events",
};

const partLabels: Record<string, string> = {
  lists: "Lists and members",
  segments: "Segments",
  templates: "Templates",
  campaigns: "Campaigns",
  flows: "Flows (as drafts)",
  forms: "Forms",
  events: "Event history",
};

const statusLabels: Record<string, string> = {
  surveying: "Looking at your account",
  ready: "Ready to import",
  importing: "Importing",
  paused: "Paused",
  syncing: "Syncing",
  done: "Done",
  cancelled: "Cancelled",
  failed: "Stopped",
};

export function KlaviyoImportPage() {
  const q = useKlaviyo();
  if (q.isPending) {
    return (
      <div className="space-y-4">
        <Skeleton className="h-40" />
        <Skeleton className="h-64" />
      </div>
    );
  }
  if (q.error) return <InlineError message={q.error.message} />;
  const s = q.data;
  const imp = s.import;
  const fresh = !imp || imp.status === "cancelled" || imp.status === "failed";
  return (
    <div className="space-y-4">
      <div>
        <h2 className="text-base font-semibold text-zinc-900">Move from Klaviyo</h2>
        <p className="mt-0.5 text-sm text-zinc-500">
          Bring profiles with their exact consent, lists, segments, event history, templates, campaigns, flows and forms across in one go. Nobody is
          emailed or texted, no flow starts, and a report shows both sides' numbers when it's done.
        </p>
      </div>
      {imp?.status === "failed" && imp.error ? (
        <Notice tone="red" icon={<XCircle />} title="The import stopped">
          {imp.error}
        </Notice>
      ) : null}
      {fresh ? <ConnectCard s={s} again={!!imp} /> : null}
      {imp?.status === "surveying" ? <SurveyingCard imp={imp} /> : null}
      {imp?.status === "ready" && imp.survey ? <SurveyCard imp={imp} survey={imp.survey} s={s} /> : null}
      {imp && ["importing", "paused", "syncing", "done"].includes(imp.status) ? <ProgressCard imp={imp} s={s} /> : null}
      {imp && (imp.status === "done" || imp.status === "syncing") && imp.compared_at ? <ReportCard imp={imp} /> : null}
      {imp && ["importing", "paused", "syncing", "done"].includes(imp.status) ? <SyncCard imp={imp} s={s} /> : null}
      {imp && (imp.status === "done" || imp.status === "syncing") ? <CampaignsCard imp={imp} /> : null}
      <ChecklistCard s={s} />
      {imp?.status === "done" && !imp.key_stored ? <ConnectCard s={s} again /> : null}
    </div>
  );
}

function ConnectCard({ s, again }: { s: KlaviyoState; again: boolean }) {
  const connect = useConnectKlaviyo();
  const [key, setKey] = useState("");
  const onSubmit = (e: FormEvent) => {
    e.preventDefault();
    connect.mutate(key.trim(), { onSuccess: () => setKey("") });
  };
  return (
    <Card title={again ? "Connect Klaviyo again" : "1. Connect your Klaviyo account"}>
      <div className="space-y-4 text-sm text-zinc-600">
        {again ? (
          <p>Running the import again only adds what's new since last time: nothing is duplicated.</p>
        ) : (
          <ol className="list-decimal space-y-1 pl-5">
            <li>
              In Klaviyo, open <span className="font-medium text-zinc-800">Settings → Account → API keys</span> and choose <span className="font-medium text-zinc-800">Create Private API Key</span>.
            </li>
            <li>
              Pick <span className="font-medium text-zinc-800">Custom key</span> and give it <span className="font-medium text-zinc-800">Read access</span> to:{" "}
              {s.scopes.map((x) => x.label).join(", ")}. DumbMail never changes anything in Klaviyo.
            </li>
            <li>Copy the key (it starts with pk_) and paste it here.</li>
          </ol>
        )}
        <form onSubmit={onSubmit} className="flex flex-wrap items-end gap-2">
          <Field label="Klaviyo private API key" className="min-w-0 flex-1">
            <Input value={key} onChange={(e) => setKey(e.target.value)} placeholder="pk_…" autoComplete="off" spellCheck={false} required maxLength={200} />
          </Field>
          <Button type="submit" variant="primary" icon={<KeyRound className="size-3.5" />} loading={connect.isPending} disabled={!key.trim()}>
            Connect
          </Button>
        </form>
        <p className="text-xs text-zinc-500">
          The key is stored encrypted only while the import runs (and while the ongoing sync is on). It's deleted when the import finishes, when you
          cancel, or when you turn the sync off.
        </p>
      </div>
    </Card>
  );
}

function SurveyingCard({ imp }: { imp: KlaviyoImport }) {
  return (
    <Card title="Looking at your Klaviyo account">
      <div className="flex items-center gap-2 text-sm text-zinc-600">
        <Loader2 className="size-4 animate-spin text-zinc-400" />
        Counting what's in {imp.klaviyo_account.name ?? "your account"}. This takes a minute or two; nothing is imported yet.
      </div>
    </Card>
  );
}

function countText(c: SurveyCount | undefined): string {
  if (!c) return "—";
  if (c.count === null) return "No access";
  return `${c.exact ? "" : "More than "}${formatNumber(c.count)}`;
}

function SurveyCard({ imp, survey, s }: { imp: KlaviyoImport; survey: KlaviyoSurvey; s: KlaviyoState }) {
  const start = useStartKlaviyo();
  const cancel = useCancelKlaviyo();
  const [what, setWhat] = useState<string[]>(s.choosable);
  const [range, setRange] = useState("2y");
  const [sync, setSync] = useState(false);
  const [confirming, setConfirming] = useState(false);
  const eventsFrom = (() => {
    if (range === "all") return "all";
    const years = Number(range.replace("y", ""));
    const d = new Date();
    d.setFullYear(d.getFullYear() - years);
    return d.toISOString().slice(0, 10);
  })();
  const rows: [string, string][] = [
    ["Profiles", countText(survey.profiles)],
    ["Lists", countText(survey.lists)],
    ["Segments", countText(survey.segments)],
    ["Metrics", countText(survey.metrics)],
    ["Events (estimate, last 2 years)", `About ${formatNumber(survey.events.estimate)}`],
    ["Templates", countText(survey.templates)],
    ["Campaigns", countText(survey.campaigns)],
    ["Flows", countText(survey.flows)],
    ["Forms", countText(survey.forms)],
    ["Suppressed emails", countText(survey.suppressions)],
    ["Catalog items (not imported)", countText(survey.catalog_items)],
    ["Coupons (not imported)", countText(survey.coupons)],
  ];
  const noProfiles = survey.missing_scopes.includes("profiles:read");
  const toggle = (k: string) => setWhat((w) => (w.includes(k) ? w.filter((x) => x !== k) : [...w, k]));
  return (
    <Card
      title={`2. What's in ${imp.klaviyo_account.name ?? "your Klaviyo account"}`}
      actions={
        <Button size="sm" variant="ghost" onClick={() => cancel.mutate()} loading={cancel.isPending}>
          Disconnect
        </Button>
      }
    >
      <div className="space-y-5">
        <dl className="grid grid-cols-1 gap-x-6 gap-y-2 text-sm sm:grid-cols-2">
          {rows.map(([label, value]) => (
            <div key={label} className="flex justify-between gap-3 border-b border-zinc-100 pb-1.5">
              <dt className="text-zinc-500">{label}</dt>
              <dd className="font-medium tabular-nums text-zinc-900">{value}</dd>
            </div>
          ))}
        </dl>
        <p className="text-sm text-zinc-600">
          Estimated time: about {formatNumber(survey.estimate_minutes)} {survey.estimate_minutes === 1 ? "minute" : "minutes"}. You can leave this page
          while it runs. Key {imp.key_hint}.
        </p>
        {survey.missing_scopes.length > 0 ? (
          <Notice tone={noProfiles ? "red" : "amber"} icon={<AlertTriangle />} title="The key can't read everything">
            Missing read access: {survey.missing_scopes.join(", ")}. {noProfiles ? "Profiles are required; give the key read access to Profiles and connect again." : "Those parts are left out unless you add the access and connect again."}
          </Notice>
        ) : null}
        <div className="space-y-3">
          <div className="text-xs font-medium text-zinc-700">What to bring across</div>
          <Checkbox label="Profiles, consent and suppressions" description="Always imported." checked disabled />
          <div className="grid grid-cols-1 gap-2 sm:grid-cols-2">
            {s.choosable.map((k) => (
              <Checkbox key={k} label={partLabels[k] ?? k} checked={what.includes(k)} onChange={() => toggle(k)} />
            ))}
          </div>
          <Field label="Event history to import" hint="Orders, checkouts, product views, carts, and email and SMS engagement. More history takes longer." className="max-w-xs">
            <Select value={range} onChange={(e) => setRange(e.target.value)} disabled={!what.includes("events")}>
              <option value="1y">Last year</option>
              <option value="2y">Last 2 years</option>
              <option value="5y">Last 5 years</option>
              <option value="all">Everything</option>
            </Select>
          </Field>
          <div className="flex items-start gap-3">
            <Toggle checked={sync} onChange={setSync} label="Keep syncing from Klaviyo" />
            <div className="text-sm">
              <div className="font-medium text-zinc-800">Keep syncing from Klaviyo every hour</div>
              <div className="text-xs text-zinc-500">For running both for a while. Keeps the key stored until you turn it off.</div>
            </div>
          </div>
        </div>
        <div className="flex justify-end">
          <Button variant="primary" onClick={() => setConfirming(true)} disabled={noProfiles}>
            Start import
          </Button>
        </div>
      </div>
      {confirming ? (
        <ConfirmDialog
          title="Start the import?"
          body="DumbMail reads your Klaviyo account and adds what it finds here. Nobody is emailed or texted, no flow starts, and imported flows stay drafts. You can pause or cancel at any time."
          confirmLabel="Start import"
          loading={start.isPending}
          onClose={() => setConfirming(false)}
          onConfirm={() => start.mutate({ confirm: true, what, events_from: eventsFrom, sync }, { onSuccess: () => setConfirming(false) })}
        />
      ) : null}
    </Card>
  );
}

function ProgressCard({ imp, s }: { imp: KlaviyoImport; s: KlaviyoState }) {
  const pause = usePauseKlaviyo();
  const cancel = useCancelKlaviyo();
  const [confirmCancel, setConfirmCancel] = useState(false);
  const { account } = useSession();
  const syncing = imp.status === "syncing";
  const steps = syncing ? ["sync_profiles", "sync_list_members", "sync_events"] : s.steps.filter((st) => imp.progress[st] || imp.phase === st);
  const running = imp.status === "importing" || syncing;
  return (
    <Card
      title={syncing ? "Syncing from Klaviyo" : "3. Import"}
      actions={
        <div className="flex items-center gap-2">
          <StatusBadge status={imp.status === "importing" || imp.status === "syncing" ? "sending" : imp.status === "done" ? "sent" : imp.status} label={statusLabels[imp.status]} />
          {imp.status === "importing" ? (
            <Button size="sm" icon={<Pause className="size-3.5" />} onClick={() => pause.mutate(false)} loading={pause.isPending}>
              Pause
            </Button>
          ) : null}
          {imp.status === "paused" ? (
            <Button size="sm" icon={<Play className="size-3.5" />} onClick={() => pause.mutate(true)} loading={pause.isPending}>
              Resume
            </Button>
          ) : null}
          {imp.status === "importing" || imp.status === "paused" ? (
            <Button size="sm" variant="danger" onClick={() => setConfirmCancel(true)}>
              Cancel
            </Button>
          ) : null}
        </div>
      }
    >
      <div className="space-y-3">
        {imp.status === "done" ? (
          <p className="text-sm text-zinc-600">
            Finished {formatDateTime(imp.finished_at, account.timezone)}. {imp.key_stored ? "The Klaviyo key is kept for the ongoing sync." : "The Klaviyo key has been deleted."}
          </p>
        ) : null}
        <ul className="divide-y divide-zinc-100 text-sm">
          {steps.map((st) => {
            const p = imp.progress[st];
            const current = imp.phase === st && running;
            return (
              <li key={st} className="flex items-center justify-between gap-3 py-2">
                <span className="flex items-center gap-2">
                  {p?.finished ? (
                    <CheckCircle2 className="size-4 text-accent-700" />
                  ) : current ? (
                    <Loader2 className="size-4 animate-spin text-zinc-400" />
                  ) : (
                    <CircleDashed className="size-4 text-zinc-300" />
                  )}
                  <span className={cx(p?.finished || current ? "text-zinc-900" : "text-zinc-500")}>{stepLabels[st] ?? st}</span>
                </span>
                <span className="text-xs tabular-nums text-zinc-500">
                  {p ? formatNumber(p.done) : ""}
                  {p && p.skipped > 0 ? ` · ${formatNumber(p.skipped)} skipped` : ""}
                </span>
              </li>
            );
          })}
        </ul>
        {(s.issues.snapshot ?? 0) + (s.issues.partial ?? 0) + (s.issues.skipped ?? 0) + (s.issues.unmapped ?? 0) > 0 ? (
          <p className="text-xs text-zinc-500">Anything that didn't come across exactly is listed under the report.</p>
        ) : null}
      </div>
      {confirmCancel ? (
        <ConfirmDialog
          title="Cancel the import?"
          body="The import stops and the Klaviyo key is deleted. Everything imported so far stays. Connecting again later picks up only what's missing."
          confirmLabel="Cancel import"
          danger
          loading={cancel.isPending}
          onClose={() => setConfirmCancel(false)}
          onConfirm={() => cancel.mutate(undefined, { onSuccess: () => setConfirmCancel(false) })}
        />
      ) : null}
    </Card>
  );
}

function cell(v: number | null, section: string): string {
  if (v === null) return "—";
  return section === "Revenue by month" ? formatCurrency(v, true) : formatNumber(v);
}

function ReportCard({ imp }: { imp: KlaviyoImport }) {
  const version = `${imp.compared_at}-${imp.last_sync_at}`;
  const report = useKlaviyoReport(true, version);
  const issues = useKlaviyoIssues(true, version);
  const [showIssues, setShowIssues] = useState(false);
  const { account } = useSession();
  const r = report.data;
  const sections: [string, ReportLine[]][] = [];
  for (const l of r?.lines ?? []) {
    const last = sections[sections.length - 1];
    if (last && last[0] === l.section) last[1].push(l);
    else sections.push([l.section, [l]]);
  }
  return (
    <Card
      title="4. Comparison report"
      actions={
        <a href="/api/klaviyo/report.csv" className="inline-flex items-center gap-1 text-xs font-medium text-accent-800 hover:underline">
          <Download className="size-3.5" /> Download CSV
        </a>
      }
    >
      {report.isPending ? (
        <Skeleton className="h-48" />
      ) : report.error ? (
        <InlineError message={report.error.message} />
      ) : r ? (
        <div className="space-y-4">
          {r.all_ok ? (
            <Notice tone="green" icon={<CheckCircle2 />} title="Everything matches">
              Every line agrees with Klaviyo, or differs only in the safe direction (someone less reachable here). Klaviyo's counts are from{" "}
              {formatDateTime(r.compared_at, account.timezone)}.
            </Notice>
          ) : (
            <Notice tone="red" icon={<AlertTriangle />} title={`${r.lines_not_ok} ${r.lines_not_ok === 1 ? "line needs" : "lines need"} a look`}>
              Each red line says what to do. Running the import again fills in anything missing.
            </Notice>
          )}
          <ul className="divide-y divide-zinc-100 rounded-lg border border-zinc-200 text-sm sm:hidden">
            {sections.map(([section, lines]) => [
              <li key={`h-${section}`} className="bg-zinc-50 px-3 py-1.5 text-xs font-semibold text-zinc-600">
                {section}
              </li>,
              ...lines.map((l, i) => (
                <li key={`${section}-${l.label}-${i}`} className="px-3 py-2">
                  <div className="flex items-start justify-between gap-2">
                    <span className="text-zinc-900">{l.label}</span>
                    {l.ok ? <Badge tone="green">OK</Badge> : <Badge tone="red">Check</Badge>}
                  </div>
                  <div className="mt-0.5 text-xs tabular-nums text-zinc-600">
                    Klaviyo {cell(l.klaviyo, section)} · DumbMail {cell(l.dumbmail, section)}
                  </div>
                  {l.note ? <div className="mt-0.5 text-xs text-zinc-500">{l.note}</div> : null}
                </li>
              )),
            ])}
          </ul>
          <Table className="hidden sm:block">
            <thead>
              <tr>
                <Th>What</Th>
                <Th className="text-right">Klaviyo</Th>
                <Th className="text-right">DumbMail</Th>
                <Th>Result</Th>
              </tr>
            </thead>
            <tbody>
              {sections.map(([section, lines]) => [
                <tr key={`h-${section}`}>
                  <td colSpan={4} className="bg-zinc-50 px-4 py-1.5 text-xs font-semibold text-zinc-600">
                    {section}
                  </td>
                </tr>,
                ...lines.map((l, i) => (
                  <tr key={`${section}-${l.label}-${i}`}>
                    <Td>
                      <div className="text-zinc-900">{l.label}</div>
                      {l.note ? <div className="mt-0.5 max-w-md text-xs text-zinc-500">{l.note}</div> : null}
                    </Td>
                    <Td className="text-right tabular-nums">{cell(l.klaviyo, section)}</Td>
                    <Td className="text-right tabular-nums">{cell(l.dumbmail, section)}</Td>
                    <Td>{l.ok ? <Badge tone="green">OK</Badge> : <Badge tone="red">Check</Badge>}</Td>
                  </tr>
                )),
              ])}
            </tbody>
          </Table>
          <div>
            <Button size="sm" variant="secondary" onClick={() => setShowIssues((v) => !v)}>
              {showIssues ? "Hide" : "Show"} what didn't come across exactly ({formatNumber(issues.data?.total ?? 0)})
            </Button>
            {showIssues ? (
              <ul className="mt-3 divide-y divide-zinc-100 rounded-lg border border-zinc-200 text-sm">
                {(issues.data?.data ?? []).map((i) => (
                  <li key={i.id} className="px-3 py-2">
                    <div className="flex flex-wrap items-center gap-2">
                      <Badge tone={i.kind === "skipped" ? "red" : i.kind === "note" ? "gray" : "amber"}>{i.kind}</Badge>
                      <span className="text-xs text-zinc-500">{stepLabels[i.step] ?? i.step}</span>
                      <span className="font-medium text-zinc-900">{i.item}</span>
                    </div>
                    <p className="mt-1 text-zinc-600">{i.message}</p>
                  </li>
                ))}
                {issues.data && issues.data.total === 0 ? <li className="px-3 py-3 text-zinc-500">Nothing: everything came across exactly.</li> : null}
              </ul>
            ) : null}
          </div>
        </div>
      ) : null}
    </Card>
  );
}

function SyncCard({ imp, s }: { imp: KlaviyoImport; s: KlaviyoState }) {
  const sync = useKlaviyoSync();
  const now = useKlaviyoSyncNow();
  const canEnable = imp.key_stored;
  return (
    <Card title="Run both for a while">
      <div className="space-y-3 text-sm text-zinc-600">
        <div className="flex items-start gap-3">
          <Toggle checked={imp.sync_enabled} onChange={(v) => sync.mutate(v)} disabled={sync.isPending || (!imp.sync_enabled && !canEnable)} label="Keep syncing from Klaviyo" />
          <div>
            <div className="font-medium text-zinc-800">Keep syncing from Klaviyo every hour</div>
            <p className="text-xs text-zinc-500">
              Pulls new and changed profiles (unsubscribes first), new list members and new events until you switch over. Turning it off deletes the
              stored key.
            </p>
          </div>
        </div>
        {imp.sync_enabled ? (
          <>
            <p>
              Last sync: {imp.last_sync_at ? formatRelative(imp.last_sync_at) : "not yet"}.
            </p>
            <p>
              <span className="font-medium text-zinc-800">How fast a Klaviyo unsubscribe arrives: </span>
              {s.sync_worst_case_delay}
            </p>
            <p className="text-xs text-zinc-500">While both run, an order your site reports to both is counted here twice. Turn the sync off once DumbMail's snippet is on your site.</p>
            <Button size="sm" icon={<RefreshCw className="size-3.5" />} onClick={() => now.mutate()} loading={now.isPending || imp.status === "syncing"} disabled={imp.status !== "done"}>
              Sync now
            </Button>
          </>
        ) : !canEnable && imp.status === "done" ? (
          <p className="text-xs text-zinc-500">The key was deleted when the import finished. Connect it again to sync.</p>
        ) : null}
      </div>
    </Card>
  );
}

function CampaignsCard({ imp }: { imp: KlaviyoImport }) {
  const q = useKlaviyoCampaigns(true, imp.finished_at);
  const { account } = useSession();
  const rows = q.data?.data ?? [];
  if (q.isPending) return <Skeleton className="h-32" />;
  if (rows.length === 0) return null;
  return (
    <Card title="Past campaigns from Klaviyo" padded={false}>
      <Table className="rounded-none border-0 shadow-none">
        <thead>
          <tr>
            <Th>Campaign</Th>
            <Th>Sent</Th>
            <Th className="text-right">Recipients</Th>
            <Th className="text-right">Opened</Th>
            <Th className="text-right">Clicked</Th>
            <Th className="text-right">Revenue</Th>
          </tr>
        </thead>
        <tbody>
          {rows.map((c) => {
            const st = c.stats ?? {};
            const base = st.delivered || st.recipients || 0;
            return (
              <tr key={c.id}>
                <Td>
                  <Link to={`/campaigns/${c.id}`} className="font-medium text-zinc-900 hover:underline">
                    {c.name}
                  </Link>
                  <div className="text-xs text-zinc-500">
                    {c.channel === "sms" ? "SMS" : "Email"}
                    {c.audience?.included.length ? ` · ${c.audience.included.join(", ")}` : ""}
                  </div>
                </Td>
                <Td>{c.status === "sent" ? formatDate(c.sent_at, account.timezone) : <StatusBadge status={c.status} />}</Td>
                <Td className="text-right tabular-nums">{st.recipients !== undefined ? formatNumber(st.recipients) : "—"}</Td>
                <Td className="text-right tabular-nums">{st.opens_unique !== undefined && base ? formatPercent(st.opens_unique / base) : "—"}</Td>
                <Td className="text-right tabular-nums">{st.clicks_unique !== undefined && base ? formatPercent(st.clicks_unique / base) : "—"}</Td>
                <Td className="text-right tabular-nums">{st.conversion_value !== undefined ? formatCurrency(st.conversion_value) : "—"}</Td>
              </tr>
            );
          })}
        </tbody>
      </Table>
    </Card>
  );
}

function ChecklistCard({ s }: { s: KlaviyoState }) {
  const mark = useMarkKlaviyo();
  const check = useCheckSiteKlaviyo();
  const hasImport = !!s.import;
  return (
    <Card title={`Switch-over checklist (${s.checklist.done} of ${s.checklist.total})`}>
      <ul className="divide-y divide-zinc-100">
        {s.checklist.items.map((i: ChecklistItem) => (
          <li key={i.key} className="flex flex-col gap-2 py-2.5 sm:flex-row sm:items-start sm:justify-between">
            <div className="flex min-w-0 items-start gap-2.5">
              {i.done ? <CheckCircle2 className="mt-0.5 size-4 shrink-0 text-accent-700" /> : <CircleDashed className="mt-0.5 size-4 shrink-0 text-zinc-300" />}
              <div className="min-w-0 text-sm">
                <div className="font-medium text-zinc-900">
                  {i.link ? (
                    <Link to={i.link} className="hover:underline">
                      {i.label}
                    </Link>
                  ) : (
                    i.label
                  )}
                </div>
                <p className="text-xs text-zinc-500">{i.detail}</p>
              </div>
            </div>
            {!i.auto && hasImport ? (
              <div className="flex shrink-0 items-center gap-2 pl-6 sm:pl-0">
                {i.key === "klaviyo_removed" ? (
                  <Button size="sm" icon={<Search className="size-3.5" />} onClick={() => check.mutate()} loading={check.isPending}>
                    Check my site
                  </Button>
                ) : null}
                <Checkbox label="Done" checked={s.import?.checklist[i.key]?.done === true} onChange={(e) => mark.mutate({ item: i.key, done: e.target.checked })} />
              </div>
            ) : null}
          </li>
        ))}
      </ul>
    </Card>
  );
}
