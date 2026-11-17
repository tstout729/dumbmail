import { useState } from "react";
import { Link } from "react-router";
import { AlertTriangle } from "lucide-react";
import { Badge, Button, Card, Checkbox, InlineError, Notice, Skeleton } from "../../components/ui";
import { formatDateTime, formatNumber } from "../../lib/format";
import { useSession } from "../../api";
import { useSetTransactionalEnabled, useTransactionalSettings } from "./api";
import type { TransactionalSettings } from "./types";

/** Settings → Transactional: turn transactional email on (owner) or off, and see its mode and limits. */
export function TransactionalSettingsPage() {
  const settings = useTransactionalSettings();
  if (settings.isPending) return <Skeleton className="h-48 w-full" />;
  if (settings.error) return <InlineError message={settings.error.message} />;
  return <TransactionalSettingsCard settings={settings.data} />;
}

export function TransactionalSettingsCard({ settings }: { settings: TransactionalSettings }) {
  const { account } = useSession();
  const set = useSetTransactionalEnabled();
  const [agreed, setAgreed] = useState(false);
  const s = settings;
  return (
    <div className="space-y-4">
      <Card
        title="Transactional email"
        actions={<Badge tone={s.mode === "off" ? "gray" : s.mode === "full" ? "green" : "amber"}>{s.mode === "off" ? "Off" : s.mode === "full" ? "On" : "On, restricted"}</Badge>}
      >
        <p className="text-sm text-zinc-600">
          Transactional email is for messages a person triggered: receipts, password resets, shipping notices. It goes out even to people who unsubscribed from
          marketing, so it must never promote anything. It never goes to addresses that bounced or marked email as spam.
        </p>
        {s.enabled ? (
          <div className="mt-4 space-y-3">
            <p className="text-sm text-zinc-600">On since {s.enabled_at ? formatDateTime(s.enabled_at, account.timezone) : "—"}.</p>
            {s.mode === "restricted" ? (
              <Notice tone="amber" icon={<AlertTriangle />} title="Restricted mode">
                Only saved{" "}
                <Link to="/transactional" className="font-medium underline">
                  transactional templates
                </Link>{" "}
                can be sent (no inline content), up to {formatNumber(s.daily_limit)} a day. For inline content and higher limits, ask{" "}
                {s.support_email ? (
                  <a href={`mailto:${s.support_email}?subject=Transactional%20email%20full%20mode`} className="font-medium underline">
                    {s.support_email}
                  </a>
                ) : (
                  "support"
                )}{" "}
                for full mode, with a line on what you send.
              </Notice>
            ) : null}
            <p className="text-xs text-zinc-500">
              Limits: {formatNumber(s.per_minute_limit)} a minute and {formatNumber(s.daily_limit)} in any 24 hours.
            </p>
            {set.error ? <p className="text-xs text-red-600">{set.error.message}</p> : null}
            <Button variant="ghost" loading={set.isPending} onClick={() => set.mutate({ enabled: false })}>
              Turn off
            </Button>
          </div>
        ) : (
          <div className="mt-4 space-y-3">
            <Checkbox
              checked={agreed}
              onChange={(e) => setAgreed(e.target.checked)}
              label="I'll only send email a person triggered, never marketing or promotions"
              description={
                s.hosted
                  ? "It starts in restricted mode: saved transactional templates only, up to " + formatNumber(s.daily_limit) + " a day. Only the account owner can turn it on."
                  : "Only the account owner can turn it on."
              }
            />
            {set.error ? <p className="text-xs text-red-600">{set.error.message}</p> : null}
            <Button variant="primary" disabled={!agreed} loading={set.isPending} onClick={() => set.mutate({ enabled: true, acknowledge: true })}>
              Turn on transactional email
            </Button>
          </div>
        )}
      </Card>
    </div>
  );
}
