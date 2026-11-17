import { InlineTitle } from "../../components/ui/InlineTitle";
import { useState } from "react";
import { useBlocker, useNavigate, useParams } from "react-router";
import { AlertTriangle, CalendarClock, CheckCircle2, Copy, FlaskConical, Send, Users, XCircle } from "lucide-react";
import type { AbTest, Audience, Campaign, EmailContent, SmsContent, VersionId } from "../../api";
import {
  useCampaign,
  useCancelCampaign,
  useDuplicateCampaign,
  useEstimate,
  useScheduleCampaign,
  useSendCampaign,
  useSending,
  isVerifiedSender,
  useSession,
  useTestCampaign,
  useUpdateCampaign,
} from "../../api";
import { AudiencePicker } from "../../components/audience/AudiencePicker";
import { EmailEditor } from "../../components/email/EmailEditor";
import { SmsEditor } from "../../components/email/SmsEditor";
import { Button, ConfirmDialog, Field, Input, Page, PageHeader, Select, Skeleton, StatusBadge, Step, cx } from "../../components/ui";
import { formatDateTime, formatNumber, pluralize } from "../../lib/format";
import { toast } from "../../lib/toast";
import { tzAbbrev, utcToZoned, zonedToUtc } from "../../lib/tz";
import { CouponSendCheck } from "../../features/coupons/CouponSendCheck";
import { AbTestSettings, AbTestStart, VersionBar } from "./AbTestPanel";
import {
  addVersion,
  afterTestSentence,
  removeVersion,
  splitSentence,
  startTest,
  testSettings,
  testSplit,
  versionContent,
  versionIds,
  versionLabel,
  waitLabel,
  withVersionContent,
} from "./abTest";
import { CampaignReportView } from "./CampaignReport";
import { SendOptionsPanel, localTimeNote } from "../../features/sendtime/SendOptionsPanel";
import { clockLabel, sendOptionsOf, timingLabel } from "../../features/sendtime/options";
import type { SendOptions } from "../../features/sendtime/types";
import { ChannelIcon } from "./CampaignsPage";

export function CampaignPage() {
  const { id = "" } = useParams();
  const campaign = useCampaign(id);
  if (campaign.isPending) {
    return (
      <Page>
        <Skeleton className="mb-2 h-7 w-64" />
        <Skeleton className="mb-6 h-4 w-40" />
        <div className="space-y-3">
          {[0, 1, 2].map((i) => (
            <Skeleton key={i} className="h-16 w-full" />
          ))}
        </div>
      </Page>
    );
  }
  if (campaign.error) {
    return (
      <Page>
        <PageHeader title="Campaign" breadcrumbs={[{ label: "Campaigns", to: "/campaigns" }]} />
        <p className="text-sm text-red-600">{campaign.error.message}</p>
      </Page>
    );
  }
  const c = campaign.data;
  if (c.status === "sent" || c.status === "sending" || c.status === "testing") return <CampaignReportView campaign={c} />;
  return <CampaignEditor key={c.id} campaign={c} />;
}

type Draft = Pick<Campaign, "name" | "audience" | "content" | "ab_test"> & { send_options: SendOptions | null };

function isEmail(_content: Campaign["content"], channel: Campaign["channel"]): _content is EmailContent {
  return channel === "email";
}

function CampaignEditor({ campaign }: { campaign: Campaign }) {
  const { account, user } = useSession();
  const navigate = useNavigate();
  const sending = useSending();
  const update = useUpdateCampaign(campaign.id);
  const schedule = useScheduleCampaign(campaign.id);
  const send = useSendCampaign(campaign.id);
  const cancel = useCancelCampaign(campaign.id);
  const duplicate = useDuplicateCampaign();
  const testSend = useTestCampaign(campaign.id);

  const [draft, setDraft] = useState<Draft | null>(null);
  const [open, setOpen] = useState<number | null>(campaign.audience.include.length === 0 ? 1 : 2);
  const [confirmSend, setConfirmSend] = useState(false);
  const [tab, setTab] = useState<VersionId>("a");
  const [removing, setRemoving] = useState<VersionId | "test" | null>(null);
  const editable = campaign.status === "draft" || campaign.status === "scheduled";
  const current: Draft = draft ?? { name: campaign.name, audience: campaign.audience, content: campaign.content, ab_test: campaign.ab_test, send_options: sendOptionsOf(campaign) };
  const test = current.ab_test;
  const versions = versionIds(test);
  // A removed version's tab falls back to A.
  const version = versions.includes(tab) ? tab : "a";
  const dirty = draft !== null;
  const tz = account.timezone;

  const blocker = useBlocker(({ currentLocation, nextLocation }) => dirty && currentLocation.pathname !== nextLocation.pathname);

  const edit = (patch: Partial<Draft>) => setDraft({ ...current, ...patch });
  const estimate = useEstimate({ channel: campaign.channel, audience: current.audience });

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

  const email = isEmail(current.content, campaign.channel) ? current.content : null;
  const sms = email ? null : (current.content as SmsContent);
  // Every version's sender must be on a verified domain.
  const domainVerified = versions.every((v) => {
    const c = versionContent(current, v);
    return isVerifiedSender(sending.data, (isEmail(c, campaign.channel) && c.from_email) || account.from_email);
  });
  const shown = versionContent(current, version);
  const editVersion = (content: EmailContent | SmsContent) => setDraft(withVersionContent(current, version, content));

  // Each version's own checks; with a test they're labelled "Version B: …".
  const versionChecks = versions.flatMap((v) => {
    const content = versionContent(current, v);
    const prefix = test ? `${versionLabel(v)}: ` : "";
    if (isEmail(content, campaign.channel)) {
      return [
        { ok: (content.subject ?? "").trim() !== "", label: `${prefix}Add a subject line`, blocking: true },
        { ok: Boolean(content.from_email || account.from_email), label: `${prefix}Set a from email (here or in Settings)`, blocking: true },
      ];
    }
    const body = (content as SmsContent).body ?? "";
    return [
      { ok: body.trim() !== "", label: `${prefix}Write your message`, blocking: true },
      { ok: /stop/i.test(body), label: `${prefix}Include opt-out language like “Reply STOP to opt out”`, blocking: false },
    ];
  });
  const checks: { ok: boolean; label: string; blocking: boolean }[] = [
    { ok: current.audience.include.length > 0, label: "Choose who receives it", blocking: true },
    ...versionChecks,
    ...(email
      ? [
          { ok: domainVerified, label: "Send from an address on a verified domain (Settings → Domains)", blocking: false },
          { ok: Boolean(account.address), label: "Add a mailing address in Settings (required by CAN-SPAM)", blocking: true },
        ]
      : []),
  ];
  const ready = checks.every((c) => c.ok || !c.blocking);
  const contentDone = versions.every((v) => {
    const content = versionContent(current, v);
    return isEmail(content, campaign.channel) ? Boolean(content.subject) : Boolean((content as SmsContent).body);
  });

  const contentSummary = [
    test ? `A/B test, ${versions.length} versions` : null,
    email ? (email.subject ? `Subject: ${email.subject}` : "No subject yet") : sms?.body ? sms.body.slice(0, 90) : "No message yet",
  ]
    .filter(Boolean)
    .join(" · ");
  const count = estimate.data?.count;

  return (
    <Page wide>
      <PageHeader
        breadcrumbs={[{ label: "Campaigns", to: "/campaigns" }]}
        title={
          <span className="flex items-center gap-2.5">
            <ChannelIcon channel={campaign.channel} />
            <InlineTitle
              value={current.name}
              onChange={(e) => edit({ name: e.target.value })}
              disabled={!editable}
              aria-label="Campaign name"
            />
            <StatusBadge status={campaign.status} />
          </span>
        }
        description={campaign.status === "scheduled" ? scheduledLine(campaign.send_at, sendOptionsOf(campaign), tz) : undefined}
        actions={
          <>
            <Button variant="ghost" icon={<Copy className="size-3.5" />} onClick={() => duplicate.mutate(campaign.id, { onSuccess: (d) => navigate(`/campaigns/${d.id}`) })}>
              Duplicate
            </Button>
            {campaign.status === "scheduled" ? (
              <Button variant="secondary" loading={cancel.isPending} onClick={() => cancel.mutate(undefined, { onSuccess: () => toast.success("Schedule cancelled") })}>
                Unschedule
              </Button>
            ) : null}
            {editable ? (
              <Button variant={dirty ? "primary" : "secondary"} disabled={!dirty} loading={update.isPending} onClick={() => void save().then((ok) => ok && toast.success("Saved"))}>
                {dirty ? "Save" : "Saved"}
              </Button>
            ) : null}
          </>
        }
      />

      {!editable ? (
        <div className="mb-4 flex items-center gap-2 rounded-md border border-zinc-200 bg-white px-3 py-2 text-sm text-zinc-600">
          <AlertTriangle className="size-4 text-amber-600" />
          This campaign is {campaign.status} and can't be edited. Duplicate it to send again.
        </div>
      ) : null}

      <div className="space-y-3">
        <Step
          index={1}
          title="Audience"
          done={current.audience.include.length > 0}
          open={open === 1}
          onToggle={() => setOpen(open === 1 ? null : 1)}
          summary={
            current.audience.include.length > 0
              ? `${current.audience.include.length} included, ${current.audience.exclude.length} excluded · ${estimate.data ? pluralize(estimate.data.count, "reachable recipient") : "estimating…"}`
              : "Choose lists or segments"
          }
        >
          <div className="grid grid-cols-1 gap-6 lg:grid-cols-[minmax(0,1fr)_260px]">
            <AudiencePicker value={current.audience} onChange={(audience: Audience) => edit({ audience })} disabled={!editable} />
            <div className="rounded-lg bg-zinc-50 p-4">
              <div className="flex items-center gap-2 text-xs font-medium text-zinc-500">
                <Users className="size-3.5" /> Reachable recipients
              </div>
              <div className={cx("mt-1 text-2xl font-semibold tabular-nums", estimate.isFetching && "opacity-50")}>
                {current.audience.include.length === 0 ? "—" : estimate.data ? formatNumber(estimate.data.count) : <Skeleton className="h-7 w-20" />}
              </div>
              <p className="mt-1 text-xs text-zinc-500">
                People with {campaign.channel === "email" ? "email" : "SMS"} consent, after exclusions and suppressions.
              </p>
              {estimate.error ? <p className="mt-2 text-xs text-red-600">{estimate.error.message}</p> : null}
            </div>
          </div>
        </Step>

        <Step index={2} title="Content" done={contentDone} open={open === 2} onToggle={() => setOpen(open === 2 ? null : 2)} summary={contentSummary}>
          {test ? (
            <>
              <AbTestSettings
                test={test}
                channel={campaign.channel}
                total={count}
                disabled={!editable}
                onChange={(ab_test) => edit({ ab_test })}
                onRemove={() => setRemoving("test")}
              />
              <VersionBar
                versions={versions}
                value={version}
                onChange={setTab}
                disabled={!editable}
                onAdd={() => {
                  const next = addVersion(current, version);
                  setDraft(next);
                  setTab(versionIds(next.ab_test).at(-1) ?? "a");
                }}
                onRemove={setRemoving}
              />
            </>
          ) : editable ? (
            <AbTestStart
              onStart={() => {
                setDraft(startTest(current));
                setTab("b");
              }}
            />
          ) : null}
          {/* Keyed by version so each tab's editor keeps its own UI state (open blocks, dialogs). */}
          {isEmail(shown, campaign.channel) ? (
            <EmailEditor key={version} value={shown} onChange={editVersion} readOnly={!editable} templateName={current.name} />
          ) : (
            <SmsEditor key={version} value={shown as SmsContent} onChange={editVersion} readOnly={!editable} templateName={current.name} />
          )}
        </Step>

        <Step index={3} title="Timing" done={current.audience.include.length > 0} open={open === 3} onToggle={() => setOpen(open === 3 ? null : 3)} summary={`${timingLabel(current.send_options)}${current.send_options?.smart_sending ? " · smart sending on" : ""}`}>
          <SendOptionsPanel
            campaignId={campaign.id}
            channel={campaign.channel}
            audience={current.audience}
            value={current.send_options}
            onChange={(send_options) => edit({ send_options })}
            disabled={!editable}
            timezone={tz}
          />
        </Step>

        <Step index={4} title="Review & send" open={open === 4} onToggle={() => setOpen(open === 4 ? null : 4)} summary={ready ? "Ready to send" : "A few things left"} done={ready && campaign.status === "scheduled"}>
          <div className="grid grid-cols-1 gap-6 lg:grid-cols-2">
            <div className="space-y-4">
              <ul className="space-y-2">
                {checks.map((c) => (
                  <li key={c.label} className="flex items-start gap-2 text-sm">
                    {c.ok ? (
                      <CheckCircle2 className="mt-0.5 size-4 shrink-0 text-accent-600" />
                    ) : c.blocking ? (
                      <XCircle className="mt-0.5 size-4 shrink-0 text-red-500" />
                    ) : (
                      <AlertTriangle className="mt-0.5 size-4 shrink-0 text-amber-500" />
                    )}
                    <span className={c.ok ? "text-zinc-500" : "text-zinc-800"}>{c.label}</span>
                  </li>
                ))}
              </ul>
              {test ? <AbSummary test={test} total={count} /> : null}
              <TestSend
                defaultTo={campaign.channel === "email" ? user.email : ""}
                channel={campaign.channel}
                versions={test ? versions : null}
                loading={testSend.isPending}
                disabled={!editable}
                onSend={async (to, v) => {
                  if (!(await save())) return;
                  testSend.mutate(
                    { to, version: test ? v : undefined },
                    { onSuccess: (r) => toast.success(`Sent ${pluralize(r.sent, "test")}${test ? ` of version ${v.toUpperCase()}` : ""}`) },
                  );
                }}
              />
            </div>
            {editable ? (
              <SendPanel
                timezone={tz}
                initial={campaign.send_at}
                disabled={!ready}
                count={count}
                test={test}
                localTime={current.send_options?.timing === "local_time" ? (current.send_options.local_time ?? "10:00") : null}
                scheduling={schedule.isPending || update.isPending}
                onSchedule={async (sendAt) => {
                  if (!(await save())) return;
                  schedule.mutate(sendAt, { onSuccess: (c) => toast.success(`Scheduled for ${formatDateTime(c.send_at, tz)}`) });
                }}
                onSendNow={() => setConfirmSend(true)}
              />
            ) : null}
          </div>
        </Step>
      </div>

      {confirmSend ? (
        <ConfirmDialog
          title="Send this campaign now?"
          body={
            <>
              {localTimeNote(current.send_options) ? `${localTimeNote(current.send_options)} ${sendNowBody(test, count)}` : sendNowBody(test, count)}
              <CouponSendCheck campaignId={campaign.id} />
            </>
          }
          confirmLabel="Send now"
          loading={send.isPending || update.isPending}
          onConfirm={async () => {
            if (!(await save())) return;
            send.mutate(undefined, {
              onSuccess: () => {
                setConfirmSend(false);
                toast.success("Sending…");
              },
            });
          }}
          onClose={() => setConfirmSend(false)}
        />
      ) : null}
      {removing ? (
        <ConfirmDialog
          title={removing === "test" ? "Remove the A/B test?" : `Remove version ${removing.toUpperCase()}?`}
          body={
            removing === "test"
              ? `${versions.length === 2 ? "Version B is" : `Versions ${versions.slice(1).map((v) => v.toUpperCase()).join(", ")} are`} deleted. Version A stays as the campaign's message.`
              : versions.length === 2
                ? "Its content is deleted, and with one version left the A/B test ends."
                : "Its content is deleted. Later versions move up a letter."
          }
          confirmLabel="Remove"
          danger
          onConfirm={() => {
            if (removing === "test") {
              edit({ ab_test: null });
            } else {
              setDraft(removeVersion(current, removing));
              const i = versions.indexOf(removing);
              setTab(versions[Math.max(0, i - 1)] ?? "a");
            }
            setRemoving(null);
          }}
          onClose={() => setRemoving(null)}
        />
      ) : null}
      {blocker.state === "blocked" ? (
        <ConfirmDialog
          title="Leave without saving?"
          body="Your changes to this campaign haven't been saved."
          confirmLabel="Discard changes"
          danger
          onConfirm={() => blocker.proceed()}
          onClose={() => blocker.reset()}
        />
      ) : null}
    </Page>
  );
}

type TestSendProps = {
  defaultTo: string;
  channel: Campaign["channel"];
  /** The A/B test's versions, or null without a test. */
  versions: VersionId[] | null;
  loading: boolean;
  disabled: boolean;
  onSend: (to: string[], version: VersionId) => void;
};

function TestSend({ defaultTo, channel, versions, loading, disabled, onSend }: TestSendProps) {
  const [to, setTo] = useState(defaultTo);
  const [picked, setPicked] = useState<VersionId>("a");
  // A version removed since it was picked falls back to A.
  const version = versions?.includes(picked) ? picked : "a";
  const list = to
    .split(/[,\s]+/)
    .map((s) => s.trim())
    .filter(Boolean);
  return (
    <div className="rounded-lg border border-zinc-200 p-3">
      <Field label="Send a test" hint={channel === "email" ? "Comma-separated email addresses." : "Comma-separated phone numbers in +1… format."}>
        <div className="flex gap-2">
          <Input value={to} onChange={(e) => setTo(e.target.value)} placeholder={channel === "email" ? "you@company.com" : "+15035550100"} disabled={disabled} />
          {versions ? (
            <Select value={version} onChange={(e) => setPicked(e.target.value as VersionId)} disabled={disabled} aria-label="Version to test" className="w-32 shrink-0">
              {versions.map((v) => (
                <option key={v} value={v}>
                  {versionLabel(v)}
                </option>
              ))}
            </Select>
          ) : null}
          <Button loading={loading} disabled={disabled || list.length === 0} onClick={() => onSend(list, version)}>
            Send test
          </Button>
        </div>
      </Field>
    </div>
  );
}

type SendPanelProps = {
  timezone: string;
  initial: string | null;
  disabled: boolean;
  count: number | undefined;
  test: AbTest | null;
  /** Set when it goes out at this hour on each person's clock: the schedule picks only the day. */
  localTime: string | null;
  scheduling: boolean;
  onSchedule: (sendAt: string) => void;
  onSendNow: () => void;
};

function defaultSendTime(timezone: string) {
  const tomorrow = new Date(Date.now() + 864e5).toISOString();
  return { date: utcToZoned(tomorrow, timezone).date, time: "10:00" };
}

function SendPanel({ timezone, initial, disabled, count, test, localTime, scheduling, onSchedule, onSendNow }: SendPanelProps) {
  const [when, setWhen] = useState(() => (initial ? utcToZoned(initial, timezone) : defaultSendTime(timezone)));
  const time = localTime ?? when.time;
  const sendAt = when.date && time ? zonedToUtc(when.date, time, timezone) : null;
  const [openedAt] = useState(() => Date.now());
  // With local time, any day from today on works: zones whose hour already passed follow the late rule.
  const inPast = sendAt !== null && (localTime ? when.date < utcToZoned(new Date(openedAt).toISOString(), timezone).date : new Date(sendAt).getTime() < openedAt);
  return (
    <div className="space-y-4 rounded-lg bg-zinc-50 p-4">
      <div>
        <div className="text-sm font-semibold text-zinc-900">Send now</div>
        <p className="mb-2 text-xs text-zinc-500">
          {test ? "The test goes out immediately." : count !== undefined ? `Goes to about ${pluralize(count, "person", "people")}.` : "Goes out immediately."}
        </p>
        <Button variant="primary" icon={<Send className="size-3.5" />} disabled={disabled} onClick={onSendNow}>
          Send now
        </Button>
      </div>
      <div className="border-t border-zinc-200 pt-4">
        <div className="text-sm font-semibold text-zinc-900">Schedule</div>
        <p className="mb-2 text-xs text-zinc-500">
          {localTime
            ? `Pick the day. It goes out at ${clockLabel(localTime)} in each person's time zone, starting when the first time zone reaches it.`
            : `Times are in your account time zone (${timezone.replace(/_/g, " ")}, ${tzAbbrev(timezone)}).`}
        </p>
        <div className="flex flex-wrap items-end gap-2">
          <Field label="Date">
            <Input type="date" value={when.date} onChange={(e) => setWhen({ ...when, date: e.target.value })} className="w-40" />
          </Field>
          {localTime ? null : (
            <Field label="Time">
              <Input type="time" value={when.time} onChange={(e) => setWhen({ ...when, time: e.target.value })} className="w-32" />
            </Field>
          )}
          <Button icon={<CalendarClock className="size-3.5" />} disabled={disabled || !sendAt || inPast} loading={scheduling} onClick={() => sendAt && onSchedule(sendAt)}>
            Schedule
          </Button>
        </div>
        {inPast ? <p className="mt-1 text-xs text-red-600">{localTime ? "Pick today or a later day." : "Pick a time in the future."}</p> : null}
      </div>
    </div>
  );
}

/** The header line for a scheduled campaign. */
function scheduledLine(sendAt: string | null, o: SendOptions | null, tz: string) {
  if (o?.timing === "local_time" && o.local_date) {
    const [y, m, d] = o.local_date.split("-").map(Number);
    const day = new Intl.DateTimeFormat("en-US", { month: "short", day: "numeric", year: "numeric", timeZone: "UTC" }).format(new Date(Date.UTC(y, m - 1, d)));
    return `Goes out ${day} at ${clockLabel(o.local_time ?? "10:00")} in each person's time zone. Starts ${formatDateTime(sendAt, tz)} ${tzAbbrev(tz)}.`;
  }
  return `Scheduled for ${formatDateTime(sendAt, tz)} ${tzAbbrev(tz)}`;
}

/** The A/B test in one box on the review step. */
function AbSummary({ test, total }: { test: AbTest; total: number | undefined }) {
  return (
    <div className="flex items-start gap-2 rounded-lg border border-violet-200 bg-violet-50/40 px-3 py-2.5 text-sm text-zinc-700">
      <FlaskConical className="mt-0.5 size-4 shrink-0 text-violet-700" />
      <div>
        <div className="font-medium text-zinc-900">A/B test with {versionIds(test).length} versions</div>
        <p className="mt-0.5">
          {splitSentence(test, total)} {afterTestSentence(test)}
        </p>
      </div>
    </div>
  );
}

function sendNowBody(test: AbTest | null, count: number | undefined) {
  if (!test) {
    return count !== undefined ? `It goes to about ${pluralize(count, "person", "people")} right away. You can't unsend it.` : "It goes out right away. You can't unsend it.";
  }
  const s = testSettings(test);
  if (s.test_percent >= 100 || count === undefined) return `${splitSentence(test, count)} The test goes out right away. You can't unsend it.`;
  const { inTest, rest } = testSplit(count, s.test_percent, versionIds(test).length);
  const after = s.pick === "auto" ? `the winner goes to the other ${formatNumber(rest)} automatically` : `you pick the winner for the other ${formatNumber(rest)}`;
  return `The test goes to about ${pluralize(inTest, "person", "people")} right away; after ${waitLabel(s.wait_hours)}, ${after}. You can't unsend it.`;
}
