import { useState, type FormEvent } from "react";
import { useSearchParams } from "react-router";
import { CheckCircle2, Copy, RefreshCw, Sparkles } from "lucide-react";
import type { Account, AccountPatch, DnsRecord } from "../../api";
import { useBilling, useSending, useSession, useSetDomain, useUpdateAccount, useVerifyDomain } from "../../api";
import {
  Badge,
  Button,
  Card,
  ColorInput,
  Field,
  Input,
  Page,
  PageHeader,
  Select,
  Skeleton,
  StatusBadge,
  Table,
  Tabs,
  Td,
  Textarea,
  Th,
} from "../../components/ui";
import { copyText } from "../../lib/clipboard";
import { formatNumber } from "../../lib/format";
import { toast } from "../../lib/toast";
import { timeZones } from "../../lib/tz";

type Tab = "account" | "sending" | "sms" | "billing";

export function SettingsPage() {
  const [params, setParams] = useSearchParams();
  const tab = (params.get("tab") as Tab | null) ?? "account";
  return (
    <Page>
      <PageHeader title="Settings" />
      <Tabs
        value={tab}
        onChange={(t) => setParams(t === "account" ? {} : { tab: t }, { replace: true })}
        tabs={[
          { value: "account", label: "Account" },
          { value: "sending", label: "Email sending" },
          { value: "sms", label: "SMS" },
          { value: "billing", label: "Billing & usage" },
        ]}
      />
      <div className="max-w-3xl">
        {tab === "account" ? <AccountTab /> : tab === "sending" ? <SendingTab /> : tab === "sms" ? <SmsTab /> : <BillingTab />}
      </div>
    </Page>
  );
}

function useSaveAccount() {
  const update = useUpdateAccount();
  const save = (patch: AccountPatch) => update.mutate(patch, { onSuccess: () => toast.success("Settings saved") });
  return { save, saving: update.isPending };
}

const str = (d: FormData, k: string) => String(d.get(k) ?? "").trim() || null;

function AccountTab() {
  const { account } = useSession();
  const { save, saving } = useSaveAccount();
  const [color, setColor] = useState(account.brand_color);
  const zones = timeZones();
  const onSubmit = (e: FormEvent<HTMLFormElement>) => {
    e.preventDefault();
    const d = new FormData(e.currentTarget);
    save({
      name: str(d, "name") ?? account.name,
      website: str(d, "website"),
      timezone: String(d.get("timezone")),
      address: str(d, "address"),
      logo_url: str(d, "logo_url"),
      brand_color: color,
      onboarding: { brand: true },
    });
  };
  return (
    <form onSubmit={onSubmit}>
      <Card title="Account">
        <div className="grid gap-4 sm:grid-cols-2">
          <Field label="Company name">
            <Input name="name" required defaultValue={account.name} />
          </Field>
          <Field label="Website">
            <Input name="website" type="url" defaultValue={account.website ?? ""} placeholder="https://" />
          </Field>
          <Field label="Time zone" hint="Used for scheduling, reports and date boundaries.">
            <Select name="timezone" defaultValue={account.timezone}>
              {(zones.includes(account.timezone) ? zones : [account.timezone, ...zones]).map((z) => (
                <option key={z} value={z}>
                  {z.replace(/_/g, " ")}
                </option>
              ))}
            </Select>
          </Field>
          <Field label="Brand color">
            <ColorInput value={color} onChange={setColor} />
          </Field>
          <Field label="Logo URL" className="sm:col-span-2">
            <Input name="logo_url" defaultValue={account.logo_url ?? ""} placeholder="https://…/logo.png" />
          </Field>
          <Field label="Mailing address" className="sm:col-span-2" hint="Required by CAN-SPAM. It's added to the footer of every marketing email.">
            <Textarea name="address" rows={2} defaultValue={account.address ?? ""} />
          </Field>
        </div>
        <div className="mt-4 flex justify-end">
          <Button type="submit" variant="primary" loading={saving}>
            Save
          </Button>
        </div>
      </Card>
    </form>
  );
}

function SendingTab() {
  const { account } = useSession();
  const { save, saving } = useSaveAccount();
  const onSubmit = (e: FormEvent<HTMLFormElement>) => {
    e.preventDefault();
    const d = new FormData(e.currentTarget);
    const from_email = str(d, "from_email");
    save({ from_name: str(d, "from_name"), from_email, reply_to: str(d, "reply_to"), onboarding: { sender: Boolean(from_email) } });
  };
  return (
    <div className="space-y-4">
      <form onSubmit={onSubmit}>
        <Card title="Default sender">
          <div className="grid gap-4 sm:grid-cols-2">
            <Field label="Sender name">
              <Input name="from_name" defaultValue={account.from_name ?? ""} placeholder={account.name} />
            </Field>
            <Field label="Sender email" hint="Use an address on your sending domain.">
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
      <DomainCard account={account} />
    </div>
  );
}

function hostnameOf(url: string | null): string {
  if (!url) return "";
  try {
    return new URL(url).hostname.replace(/^www\./, "");
  } catch {
    return "";
  }
}

function DomainCard({ account }: { account: Account }) {
  const sending = useSending();
  const setDomain = useSetDomain();
  const verify = useVerifyDomain();
  const [changing, setChanging] = useState(false);
  const email = sending.data?.email;
  const suggested = account.from_email?.split("@")[1] ?? hostnameOf(account.website);

  const onSubmit = (e: FormEvent<HTMLFormElement>) => {
    e.preventDefault();
    const domain = String(new FormData(e.currentTarget).get("domain")).trim().toLowerCase();
    setDomain.mutate(domain, { onSuccess: () => setChanging(false) });
  };

  return (
    <Card
      title="Sending domain"
      actions={email && email.status !== "not_started" ? <StatusBadge status={email.status} /> : null}
    >
      {sending.isPending ? (
        <Skeleton className="h-24" />
      ) : sending.error ? (
        <p className="text-sm text-red-600">{sending.error.message}</p>
      ) : !email?.domain || changing ? (
        <form onSubmit={onSubmit} className="space-y-3">
          <p className="text-sm text-zinc-600">
            Send from your own domain so inbox providers trust you. We'll give you a few DNS records to add at your domain host.
          </p>
          <div className="flex gap-2">
            <Input name="domain" required defaultValue={email?.domain ?? suggested} placeholder="yourdomain.com" className="max-w-xs" />
            <Button type="submit" variant="primary" loading={setDomain.isPending}>
              Get DNS records
            </Button>
            {changing ? (
              <Button variant="ghost" onClick={() => setChanging(false)}>
                Cancel
              </Button>
            ) : null}
          </div>
        </form>
      ) : (
        <div className="space-y-4">
          <div className="flex flex-wrap items-center justify-between gap-2">
            <div>
              <div className="text-sm font-semibold text-zinc-900">{email.domain}</div>
              <div className="text-xs text-zinc-500">
                Provider: {email.provider}
                {email.status === "verified" ? " · Ready to send" : " · Add these records, then check DNS. Changes can take up to an hour."}
              </div>
            </div>
            <div className="flex gap-2">
              <Button variant="ghost" size="sm" onClick={() => setChanging(true)}>
                Change domain
              </Button>
              <Button
                size="sm"
                icon={<RefreshCw className="size-3.5" />}
                loading={verify.isPending}
                onClick={() =>
                  verify.mutate(undefined, {
                    onSuccess: (r) =>
                      r.email.status === "verified" ? toast.success("Domain verified") : toast.info("Not verified yet. DNS changes can take a while."),
                  })
                }
              >
                Check DNS
              </Button>
            </div>
          </div>
          {email.status === "verified" ? (
            <div className="flex items-center gap-2 rounded-md bg-accent-50 px-3 py-2 text-sm text-accent-800">
              <CheckCircle2 className="size-4" /> Your domain is verified.
            </div>
          ) : null}
          <DnsTable records={email.dns_records} />
        </div>
      )}
    </Card>
  );
}

function DnsTable({ records }: { records: DnsRecord[] }) {
  return (
    <Table className="shadow-none">
      <thead>
        <tr>
          <Th>Type</Th>
          <Th>Name</Th>
          <Th>Value</Th>
          <Th>Status</Th>
        </tr>
      </thead>
      <tbody>
        {records.map((r, i) => (
          <tr key={`${r.type}-${r.name}-${i}`}>
            <Td>
              <Badge>{r.type}</Badge>
            </Td>
            <Td className="max-w-[180px]">
              <CopyCell value={r.name} />
            </Td>
            <Td className="max-w-[280px]">
              <CopyCell value={r.value} />
            </Td>
            <Td>
              <StatusBadge status={r.status} />
            </Td>
          </tr>
        ))}
      </tbody>
    </Table>
  );
}

function CopyCell({ value }: { value: string }) {
  return (
    <button type="button" onClick={() => void copyText(value)} className="group flex w-full items-center gap-1.5 text-left" title="Copy">
      <code className="min-w-0 flex-1 truncate font-mono text-xs text-zinc-800">{value}</code>
      <Copy className="size-3 shrink-0 text-zinc-300 group-hover:text-zinc-600" />
    </button>
  );
}

function SmsTab() {
  const { account } = useSession();
  const sending = useSending();
  const { save, saving } = useSaveAccount();
  const sms = sending.data?.sms;
  return (
    <form
      onSubmit={(e) => {
        e.preventDefault();
        save({ sms_from: str(new FormData(e.currentTarget), "sms_from") });
      }}
    >
      <Card title="SMS" actions={sms ? <StatusBadge status={sms.status} /> : null}>
        <div className="space-y-4">
          {sending.isPending ? (
            <Skeleton className="h-10" />
          ) : sms ? (
            <div className="grid grid-cols-2 gap-3 rounded-lg bg-zinc-50 p-3 text-sm">
              <div>
                <div className="text-xs text-zinc-500">Provider</div>
                <div className="font-medium text-zinc-900">{sms.provider || "Not connected"}</div>
              </div>
              <div>
                <div className="text-xs text-zinc-500">Provider number</div>
                <div className="font-medium tabular-nums text-zinc-900">{sms.from_number ?? "—"}</div>
              </div>
            </div>
          ) : null}
          <Field label="From number" hint="E.164 format, e.g. +15035550100. Must be a number your SMS provider has provisioned for you.">
            <Input name="sms_from" defaultValue={account.sms_from ?? ""} placeholder="+15035550100" className="max-w-xs" />
          </Field>
          <p className="text-xs text-zinc-500">
            Provider credentials (Telnyx or Twilio) are set on the server. Inbound STOP replies unsubscribe people automatically.
          </p>
        </div>
        <div className="mt-4 flex justify-end">
          <Button type="submit" variant="primary" loading={saving}>
            Save
          </Button>
        </div>
      </Card>
    </form>
  );
}

function Meter({ label, used, limit }: { label: string; used: number; limit: number | null }) {
  const pct = limit ? Math.min(100, (used / limit) * 100) : 0;
  return (
    <div>
      <div className="mb-1 flex justify-between text-sm">
        <span className="text-zinc-700">{label}</span>
        <span className="tabular-nums text-zinc-900">
          {formatNumber(used)} <span className="text-zinc-500">/ {limit === null || limit === undefined ? "unlimited" : formatNumber(limit)}</span>
        </span>
      </div>
      <div className="h-2 overflow-hidden rounded-full bg-zinc-100" role="meter" aria-valuenow={used} aria-valuemax={limit ?? undefined} aria-label={label}>
        <div className={pct > 90 ? "h-full rounded-full bg-red-500" : pct > 75 ? "h-full rounded-full bg-amber-500" : "h-full rounded-full bg-accent-600"} style={{ width: `${pct}%` }} />
      </div>
    </div>
  );
}

const planLabels: Record<string, string> = { free: "Free", pro: "Pro", scale: "Scale", self_hosted: "Self-hosted" };

function BillingTab() {
  const billing = useBilling();
  const b = billing.data;
  return (
    <Card title="Plan & usage">
      {billing.isPending ? (
        <Skeleton className="h-40" />
      ) : billing.error || !b ? (
        <p className="text-sm text-red-600">{billing.error?.message}</p>
      ) : (
        <div className="space-y-5">
          <div className="flex flex-wrap items-center justify-between gap-3">
            <div>
              <div className="text-xs text-zinc-500">Current plan</div>
              <div className="text-lg font-semibold text-zinc-950">{planLabels[b.plan] ?? b.plan}</div>
            </div>
            {b.plan !== "self_hosted" ? (
              <Button variant="primary" icon={<Sparkles className="size-3.5" />} onClick={() => toast.info("Paid plans are coming soon. Pricing will be a fraction of what you pay today.")}>
                Upgrade
              </Button>
            ) : null}
          </div>
          <Meter label="Profiles" used={b.usage.profiles} limit={b.limits.profiles} />
          <Meter label="Emails this month" used={b.usage.emails_this_month} limit={b.limits.emails_per_month} />
          <Meter label="SMS this month" used={b.usage.sms_this_month} limit={b.limits.sms_per_month} />
        </div>
      )}
    </Card>
  );
}
