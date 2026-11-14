import { InlineTitle } from "../../components/ui/InlineTitle";
import { useState } from "react";
import { useBlocker, useNavigate, useParams } from "react-router";
import { AlertTriangle, CalendarClock, CheckCircle2, Copy, Send, Users, XCircle } from "lucide-react";
import type { Audience, Campaign, EmailContent, SmsContent } from "../../api";
import {
  useCampaign,
  useCancelCampaign,
  useDuplicateCampaign,
  useEstimate,
  useScheduleCampaign,
  useSendCampaign,
  useSending,
  useSession,
  useTestCampaign,
  useUpdateCampaign,
} from "../../api";
import { AudiencePicker } from "../../components/audience/AudiencePicker";
import { EmailEditor } from "../../components/email/EmailEditor";
import { SmsEditor } from "../../components/email/SmsEditor";
import { Button, ConfirmDialog, Field, Input, Page, PageHeader, Skeleton, StatusBadge, Step, cx } from "../../components/ui";
import { formatDateTime, formatNumber, pluralize } from "../../lib/format";
import { toast } from "../../lib/toast";
import { tzAbbrev, utcToZoned, zonedToUtc } from "../../lib/tz";
import { CampaignReportView } from "./CampaignReport";
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
  if (c.status === "sent" || c.status === "sending") return <CampaignReportView campaign={c} />;
  return <CampaignEditor key={c.id} campaign={c} />;
}

type Draft = Pick<Campaign, "name" | "audience" | "content">;

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
  const test = useTestCampaign(campaign.id);

  const [draft, setDraft] = useState<Draft | null>(null);
  const [open, setOpen] = useState<number | null>(campaign.audience.include.length === 0 ? 1 : 2);
  const [confirmSend, setConfirmSend] = useState(false);
  const editable = campaign.status === "draft" || campaign.status === "scheduled";
  const current: Draft = draft ?? { name: campaign.name, audience: campaign.audience, content: campaign.content };
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
  const fromEmail = email ? email.from_email || account.from_email : null;
  const domainVerified = sending.data?.email.status === "verified";

  const checks: { ok: boolean; label: string; blocking: boolean }[] = [
    { ok: current.audience.include.length > 0, label: "Choose who receives it", blocking: true },
    ...(email
      ? [
          { ok: email.subject.trim() !== "", label: "Add a subject line", blocking: true },
          { ok: Boolean(fromEmail), label: "Set a from email (here or in Settings)", blocking: true },
          { ok: domainVerified, label: "Verify your sending domain for best deliverability", blocking: false },
          { ok: Boolean(account.address), label: "Add a mailing address in Settings (required by CAN-SPAM)", blocking: true },
        ]
      : [
          { ok: (sms?.body ?? "").trim() !== "", label: "Write your message", blocking: true },
          { ok: /stop/i.test(sms?.body ?? ""), label: "Include opt-out language like “Reply STOP to opt out”", blocking: false },
        ]),
  ];
  const ready = checks.every((c) => c.ok || !c.blocking);

  const contentSummary = email
    ? email.subject
      ? `Subject: ${email.subject}`
      : "No subject yet"
    : sms?.body
      ? sms.body.slice(0, 90)
      : "No message yet";

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
        description={campaign.status === "scheduled" ? `Scheduled for ${formatDateTime(campaign.send_at, tz)} ${tzAbbrev(tz)}` : undefined}
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
          <div className="grid gap-6 lg:grid-cols-[1fr_260px]">
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

        <Step index={2} title="Content" done={campaign.channel === "email" ? Boolean(email?.subject) : Boolean(sms?.body)} open={open === 2} onToggle={() => setOpen(open === 2 ? null : 2)} summary={contentSummary}>
          {email ? (
            <EmailEditor value={email} onChange={(content) => edit({ content })} readOnly={!editable} templateName={current.name} />
          ) : (
            <SmsEditor value={sms ?? { body: "" }} onChange={(content) => edit({ content })} readOnly={!editable} templateName={current.name} />
          )}
        </Step>

        <Step index={3} title="Review & send" open={open === 3} onToggle={() => setOpen(open === 3 ? null : 3)} summary={ready ? "Ready to send" : "A few things left"} done={ready && campaign.status === "scheduled"}>
          <div className="grid gap-6 lg:grid-cols-2">
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
              <TestSend
                defaultTo={campaign.channel === "email" ? user.email : ""}
                channel={campaign.channel}
                loading={test.isPending}
                disabled={!editable}
                onSend={async (to) => {
                  if (!(await save())) return;
                  test.mutate(to, { onSuccess: (r) => toast.success(`Sent ${pluralize(r.sent, "test")}`) });
                }}
              />
            </div>
            {editable ? (
              <SendPanel
                timezone={tz}
                initial={campaign.send_at}
                disabled={!ready}
                count={estimate.data?.count}
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
            estimate.data
              ? `It goes to about ${pluralize(estimate.data.count, "person", "people")} right away. You can't unsend it.`
              : "It goes out right away. You can't unsend it."
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

type TestSendProps = { defaultTo: string; channel: Campaign["channel"]; loading: boolean; disabled: boolean; onSend: (to: string[]) => void };

function TestSend({ defaultTo, channel, loading, disabled, onSend }: TestSendProps) {
  const [to, setTo] = useState(defaultTo);
  const list = to
    .split(/[,\s]+/)
    .map((s) => s.trim())
    .filter(Boolean);
  return (
    <div className="rounded-lg border border-zinc-200 p-3">
      <Field label="Send a test" hint={channel === "email" ? "Comma-separated email addresses." : "Comma-separated phone numbers in +1… format."}>
        <div className="flex gap-2">
          <Input value={to} onChange={(e) => setTo(e.target.value)} placeholder={channel === "email" ? "you@company.com" : "+15035550100"} disabled={disabled} />
          <Button loading={loading} disabled={disabled || list.length === 0} onClick={() => onSend(list)}>
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
  scheduling: boolean;
  onSchedule: (sendAt: string) => void;
  onSendNow: () => void;
};

function defaultSendTime(timezone: string) {
  const tomorrow = new Date(Date.now() + 864e5).toISOString();
  return { date: utcToZoned(tomorrow, timezone).date, time: "10:00" };
}

function SendPanel({ timezone, initial, disabled, count, scheduling, onSchedule, onSendNow }: SendPanelProps) {
  const [when, setWhen] = useState(() => (initial ? utcToZoned(initial, timezone) : defaultSendTime(timezone)));
  const sendAt = when.date && when.time ? zonedToUtc(when.date, when.time, timezone) : null;
  const [openedAt] = useState(() => Date.now());
  const inPast = sendAt !== null && new Date(sendAt).getTime() < openedAt;
  return (
    <div className="space-y-4 rounded-lg bg-zinc-50 p-4">
      <div>
        <div className="text-sm font-semibold text-zinc-900">Send now</div>
        <p className="mb-2 text-xs text-zinc-500">{count !== undefined ? `Goes to about ${pluralize(count, "person", "people")}.` : "Goes out immediately."}</p>
        <Button variant="primary" icon={<Send className="size-3.5" />} disabled={disabled} onClick={onSendNow}>
          Send now
        </Button>
      </div>
      <div className="border-t border-zinc-200 pt-4">
        <div className="text-sm font-semibold text-zinc-900">Schedule</div>
        <p className="mb-2 text-xs text-zinc-500">
          Times are in your account time zone ({timezone.replace(/_/g, " ")}, {tzAbbrev(timezone)}).
        </p>
        <div className="flex flex-wrap items-end gap-2">
          <Field label="Date">
            <Input type="date" value={when.date} onChange={(e) => setWhen({ ...when, date: e.target.value })} className="w-40" />
          </Field>
          <Field label="Time">
            <Input type="time" value={when.time} onChange={(e) => setWhen({ ...when, time: e.target.value })} className="w-32" />
          </Field>
          <Button icon={<CalendarClock className="size-3.5" />} disabled={disabled || !sendAt || inPast} loading={scheduling} onClick={() => sendAt && onSchedule(sendAt)}>
            Schedule
          </Button>
        </div>
        {inPast ? <p className="mt-1 text-xs text-red-600">Pick a time in the future.</p> : null}
      </div>
    </div>
  );
}
