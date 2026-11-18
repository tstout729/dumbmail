import type { FormEvent } from "react";
import { Link } from "react-router";
import { ArrowRight } from "lucide-react";
import { useSending, useSession } from "../../api";
import { Button, Card, Field, Input, StatusBadge, Toggle } from "../../components/ui";
import { formatNumber } from "../../lib/format";
import { SendingReviewCard } from "../../features/screening/SendingReviewCard";
import { formString, useSaveAccount } from "./shared";

export function SendingSettingsPage() {
  const { account } = useSession();
  const { save, saving } = useSaveAccount();
  const sending = useSending();
  const email = sending.data?.email;
  const verified = email?.domains.filter((d) => d.status === "verified").map((d) => d.domain) ?? [];
  const onSubmit = (e: FormEvent<HTMLFormElement>) => {
    e.preventDefault();
    const d = new FormData(e.currentTarget);
    const from_email = formString(d, "from_email");
    save({ from_name: formString(d, "from_name"), from_email, reply_to: formString(d, "reply_to"), onboarding: { sender: Boolean(from_email) } });
  };
  return (
    <div className="space-y-4">
      <SendingReviewCard />
      <form onSubmit={onSubmit}>
        <Card title="Default sender">
          <div className="grid grid-cols-1 gap-4 sm:grid-cols-2">
            <Field label="Sender name">
              <Input name="from_name" defaultValue={account.from_name ?? ""} placeholder={account.name} />
            </Field>
            <Field label="Sender email" hint={verified.length ? `Use an address on ${verified.join(", ")}.` : "Use an address on a domain you verify in Settings → Domains."}>
              <Input name="from_email" type="email" defaultValue={account.from_email ?? ""} placeholder="hello@yourdomain.com" />
            </Field>
            <Field label="Reply-to (optional)" className="sm:col-span-2">
              <Input name="reply_to" type="email" defaultValue={account.reply_to ?? ""} placeholder="support@yourdomain.com" />
            </Field>
          </div>
          <div className="mt-4 flex justify-end">
            <Button type="submit" variant="primary" loading={saving}>
              Save
            </Button>
          </div>
        </Card>
      </form>
      <Card title="Sending health">
        <div className="space-y-4">
          {sending.data?.email_limit ? <DailyLimit limit={sending.data.email_limit} /> : null}
          <div className="flex items-start justify-between gap-4">
            <div className="min-w-0">
              <div className="text-sm font-medium text-zinc-900">Skip people who never engage</div>
              <p className="text-xs text-zinc-500">
                Campaigns skip anyone who got {sending.data?.unengaged_skip.min_emails ?? 10} or more emails in the last{" "}
                {sending.data?.unengaged_skip.days ?? 180} days and didn't open, click, visit or buy. Mailing them is what slowly moves a domain to the spam
                folder. Flows still reach them.
              </p>
            </div>
            <Toggle
              checked={account.email_sunset_enabled}
              onChange={(v) => save({ email_sunset_enabled: v })}
              label="Skip people who never engage"
              disabled={saving}
            />
          </div>
        </div>
      </Card>
      <Link
        to="/settings/domains"
        className="group flex items-center gap-3 rounded-lg border border-zinc-200 bg-white px-4 py-3 shadow-xs hover:border-zinc-300"
      >
        <span className="min-w-0 flex-1">
          <span className="block text-sm font-semibold text-zinc-900">Sending domains</span>
          <span className="block text-xs text-zinc-500">
            {email?.domains.length ? email.domains.map((d) => d.domain).join(", ") : "Verify your domain so your emails land in the inbox."}
          </span>
        </span>
        {email && email.domains.length ? (
          <StatusBadge status={verified.length === email.domains.length ? "verified" : "pending"} label={`${verified.length} of ${email.domains.length} verified`} />
        ) : null}
        <ArrowRight className="size-4 text-zinc-300 group-hover:text-zinc-600" />
      </Link>
    </div>
  );
}

type Limit = { limit: number; sent_last_24h: number; remaining: number; fixed: boolean; start: number };

function DailyLimit({ limit }: { limit: Limit }) {
  const used = Math.min(100, limit.limit > 0 ? (limit.sent_last_24h / limit.limit) * 100 : 100);
  return (
    <div>
      <div className="flex items-baseline justify-between gap-2">
        <span className="text-sm font-medium text-zinc-900">Emails in the last 24 hours</span>
        <span className="text-sm tabular-nums text-zinc-700">
          {formatNumber(limit.sent_last_24h)} of {formatNumber(limit.limit)}
        </span>
      </div>
      <div className="mt-2 h-1.5 overflow-hidden rounded-full bg-zinc-100" aria-hidden="true">
        <div className="h-full rounded-full bg-accent-600" style={{ width: `${used}%` }} />
      </div>
      <p className="mt-2 text-xs text-zinc-500">
        {limit.fixed
          ? "Your limit was set by DumbMail support."
          : `New accounts start at ${formatNumber(limit.start)} emails a day. Each day your limit becomes twice your busiest recent day, as long as few emails bounce or get marked as spam. Emails past the limit wait and go out as room opens up.`}
      </p>
    </div>
  );
}
