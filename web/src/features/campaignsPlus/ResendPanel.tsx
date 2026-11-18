import { useState, type FormEvent } from "react";
import { Link, useNavigate } from "react-router";
import { Repeat } from "lucide-react";
import type { Campaign, EmailContent } from "../../api";
import { useSession } from "../../api";
import { Button, Card, Field, InlineError, Input, Modal, Notice, Segmented, Stat, StatusBadge } from "../../components/ui";
import { formatDateTime, formatNumber, formatPercent } from "../../lib/format";
import { toast } from "../../lib/toast";
import { useCreateResend, useResendInfo } from "./api";
import type { ResendInfo, ResendTarget } from "./types";

const targetLabel: Record<ResendTarget, string> = { no_open: "didn't open", no_click: "didn't click" };

/** Is this campaign's audience a resend's (who didn't open or click another campaign)? */
export function isResendAudience(c: Pick<Campaign, "audience">) {
  return c.audience.include.some((r) => (r.type as string) === "resend");
}

/** On a sent email campaign's report: its resends, totals across them, and the button to make one. */
export function ResendPanel({ campaign }: { campaign: Campaign }) {
  const info = useResendInfo(campaign.id, campaign.channel === "email" && campaign.status === "sent");
  const { account } = useSession();
  const [open, setOpen] = useState(false);
  if (campaign.channel !== "email" || campaign.status !== "sent" || !info.data) return null;
  const d = info.data;
  if (d.resend_of) return <ResendOfNotice info={d} />;
  const can = d.can_resend;
  const resends = d.resends ?? [];
  return (
    <Card
      className="mb-4"
      title="Resends"
      actions={
        <Button size="sm" icon={<Repeat className="size-3.5" />} disabled={!can?.ok} onClick={() => setOpen(true)}>
          Resend to non-openers
        </Button>
      }
    >
      {!can?.ok && can?.reason ? (
        <p className="text-sm text-zinc-600">
          {can.reason}
          {can.earliest_at && can.reason.startsWith("Wait") ? ` You can resend from ${formatDateTime(can.earliest_at, account.timezone)}.` : ""}
        </p>
      ) : resends.length === 0 ? (
        <p className="text-sm text-zinc-600">
          Send it again to the people who didn't open or click it. You can resend up to {can?.resends_allowed ?? 2} times
          {can?.latest_at ? `, until ${formatDateTime(can.latest_at, account.timezone)}` : ""}.
        </p>
      ) : null}
      {resends.length > 0 ? (
        <div className="space-y-3">
          <ul className="divide-y divide-zinc-100">
            {resends.map((r) => (
              <li key={r.id} className="flex flex-wrap items-center justify-between gap-2 py-2 text-sm">
                <Link to={`/campaigns/${r.id}`} className="font-medium text-zinc-900 hover:underline">
                  {r.name}
                </Link>
                <span className="flex items-center gap-3 text-zinc-600">
                  <span>To people who {targetLabel[r.target]}</span>
                  {r.sent_at ? <span className="tabular-nums">{formatNumber(r.stats.recipients)} sent · {formatPercent(r.stats.open_rate)} opened</span> : null}
                  <StatusBadge status={r.status} />
                </span>
              </li>
            ))}
          </ul>
          {d.combined ? (
            <div>
              <div className="mb-2 text-xs font-medium text-zinc-500">This campaign and its resends together (each person counted once)</div>
              <div className="grid grid-cols-2 gap-3 md:grid-cols-4">
                <Stat label="People reached" value={formatNumber(d.combined.people_reached)} sub={`${formatNumber(d.combined.emails_delivered)} emails delivered`} />
                <Stat label="Opened" value={formatPercent(d.combined.open_rate)} sub={`${formatNumber(d.combined.people_opened)} people`} />
                <Stat label="Clicked" value={formatPercent(d.combined.click_rate)} sub={`${formatNumber(d.combined.people_clicked)} people`} />
              </div>
            </div>
          ) : null}
        </div>
      ) : null}
      {open ? <ResendDialog campaign={campaign} note={d.undercount_note} onClose={() => setOpen(false)} /> : null}
    </Card>
  );
}

function ResendOfNotice({ info }: { info: ResendInfo }) {
  const of = info.resend_of;
  if (!of) return null;
  return (
    <div className="mb-4">
      <Notice tone="blue" title={<>Resend of <Link to={`/campaigns/${of.id}`} className="underline">{of.name}</Link></>}>
        Sent to the people who {targetLabel[of.target]} it.
      </Notice>
    </div>
  );
}

function ResendDialog({ campaign, note, onClose }: { campaign: Campaign; note: string; onClose: () => void }) {
  const content = campaign.content as EmailContent;
  const create = useCreateResend(campaign.id);
  const navigate = useNavigate();
  const [target, setTarget] = useState<ResendTarget>("no_open");
  const [subject, setSubject] = useState(content.subject ?? "");
  const [preheader, setPreheader] = useState(content.preheader ?? "");
  const onSubmit = (e: FormEvent<HTMLFormElement>) => {
    e.preventDefault();
    create.mutate(
      { target, subject: subject.trim() || undefined, preheader: preheader.trim() || undefined },
      {
        onSuccess: (c) => {
          toast.success("Resend draft created. Review it, then send.");
          navigate(`/campaigns/${c.id}`);
        },
      },
    );
  };
  return (
    <Modal title="Resend to people who didn't engage" onClose={onClose} size="md">
      <form onSubmit={onSubmit} className="space-y-4">
        <Field label="Send to people who">
          <Segmented<ResendTarget>
            value={target}
            onChange={setTarget}
            options={[
              { value: "no_open", label: "didn't open" },
              { value: "no_click", label: "didn't click" },
            ]}
          />
        </Field>
        <p className="text-sm text-zinc-600">
          This makes a draft with the same email. Nothing is sent until you send it. Who gets it is worked out when it starts sending: everyone this campaign
          reached who still hasn't {target === "no_open" ? "opened" : "clicked"} it and is still subscribed. Lists and segments don't apply.
        </p>
        <Field label="Subject" hint="A new subject often helps.">
          <Input value={subject} onChange={(e) => setSubject(e.target.value)} maxLength={300} required />
        </Field>
        <Field label="Preview text">
          <Input value={preheader} onChange={(e) => setPreheader(e.target.value)} maxLength={300} />
        </Field>
        <Notice tone="amber" title="Some readers will get it twice">
          {note}
        </Notice>
        <InlineError message={create.error?.message} />
        <div className="flex justify-end gap-2">
          <Button variant="ghost" onClick={onClose}>
            Cancel
          </Button>
          <Button type="submit" variant="primary" loading={create.isPending}>
            Create resend draft
          </Button>
        </div>
      </form>
    </Modal>
  );
}

/** In a resend's editor, in place of the list and segment picker. */
export function ResendAudience({ campaignId }: { campaignId: string }) {
  const info = useResendInfo(campaignId);
  const d = info.data;
  return (
    <div className="space-y-3 text-sm text-zinc-700">
      {d?.resend_of ? (
        <p>
          Resend of{" "}
          <Link to={`/campaigns/${d.resend_of.id}`} className="font-medium underline">
            {d.resend_of.name}
          </Link>
          .
        </p>
      ) : null}
      <p>{d?.audience_note ?? "Everyone the original reached who hasn't engaged with it when this starts sending, and is still subscribed."}</p>
      {d ? (
        <Notice tone="amber" title="Opens are undercounted">
          {d.undercount_note}
        </Notice>
      ) : null}
    </div>
  );
}
