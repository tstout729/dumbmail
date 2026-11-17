import { useState, type FormEvent, type ReactNode } from "react";
import { redirect, type LoaderFunctionArgs } from "react-router";
import { AlertTriangle, CreditCard, Server } from "lucide-react";
import type { AutoRecharge, Billing } from "../../api";
import { keys, queryClient, useAutoRecharge, useBilling, useCheckout, useSession } from "../../api";
import { TimeSeriesChart } from "../../components/charts/TimeSeriesChart";
import {
  Badge,
  Button,
  Card,
  EmptyRow,
  Field,
  Input,
  Notice,
  Segmented,
  Skeleton,
  Table,
  Td,
  Th,
  Toggle,
  cx,
} from "../../components/ui";
import { formatCurrency, formatDateTime, formatNumber, formatPrice } from "../../lib/format";
import { toast } from "../../lib/toast";
import { utcToZoned } from "../../lib/tz";

/**
 * Stripe Checkout returns to `?topup=success|cancelled`. Handle it before the page renders: toast, refresh the
 * balance (again a few seconds later, since Stripe's webhook can land after the redirect), and drop the param.
 */
export function billingLoader({ request }: LoaderFunctionArgs) {
  const url = new URL(request.url);
  const topup = url.searchParams.get("topup");
  if (!topup) return null;
  if (topup === "success") {
    toast.success("Payment received. Your credit is being added.");
    void queryClient.invalidateQueries({ queryKey: keys.billing });
    setTimeout(() => void queryClient.invalidateQueries({ queryKey: keys.billing }), 4000);
  } else if (topup === "cancelled") {
    toast.info("Top-up cancelled. You weren't charged.");
  }
  url.searchParams.delete("topup");
  return redirect(url.pathname + url.search);
}

export function BillingPage() {
  const billing = useBilling();
  if (billing.isPending) {
    return (
      <div className="space-y-4">
        <div className="grid grid-cols-1 gap-4 md:grid-cols-2">
          <Skeleton className="h-56" />
          <Skeleton className="h-56" />
        </div>
        <Skeleton className="h-32" />
        <Skeleton className="h-64" />
      </div>
    );
  }
  if (billing.error) return <p className="text-sm text-red-600">{billing.error.message}</p>;
  const b = billing.data;
  if (b.mode === "self_hosted") return <SelfHostedCard b={b} />;
  return (
    <div className="space-y-4">
      <div className="grid grid-cols-1 gap-4 md:grid-cols-2">
        <BalanceCard b={b} />
        <AutoRechargeCard b={b} />
      </div>
      <ThisMonthCard b={b} />
      <UsageCard b={b} />
      <PricesCard b={b} />
      <LedgerCard b={b} />
    </div>
  );
}

function SelfHostedCard({ b }: { b: Billing }) {
  return (
    <Card title="Billing">
      <div className="flex items-start gap-3">
        <div className="rounded-full bg-zinc-100 p-2.5 text-zinc-600">
          <Server className="size-5" />
        </div>
        <div className="space-y-1 text-sm">
          <div className="font-semibold text-zinc-900">Self-hosted: no billing</div>
          <p className="text-zinc-600">
            This server never charges for sending. You pay your email and SMS providers directly.
          </p>
          <p className="text-zinc-500">
            This month: {formatNumber(b.emails_this_month)} emails · {formatNumber(b.sms_this_month)} texts
          </p>
        </div>
      </div>
    </Card>
  );
}

const presets = [10, 25, 50, 100] as const;
const MIN_TOPUP = 10;
const MAX_TOPUP = 5000;

function BalanceCard({ b }: { b: Billing }) {
  const checkout = useCheckout();
  const [preset, setPreset] = useState<number | "custom">(25);
  const [custom, setCustom] = useState("");
  const [redirecting, setRedirecting] = useState(false);
  const amount = preset === "custom" ? Number(custom) : preset;
  const valid = Number.isFinite(amount) && amount >= MIN_TOPUP && amount <= MAX_TOPUP;

  const onSubmit = (e: FormEvent<HTMLFormElement>) => {
    e.preventDefault();
    if (!valid) return;
    checkout.mutate(amount, {
      onSuccess: ({ url }) => {
        setRedirecting(true);
        window.location.assign(url);
      },
    });
  };

  return (
    <Card title="Balance">
      <div className={cx("text-4xl font-semibold tracking-tight tabular-nums", b.balance <= 0 ? "text-red-700" : "text-zinc-950")}>
        {formatCurrency(b.balance, true)}
      </div>
      <p className="mt-1 text-xs text-zinc-500">Prepaid credit. Each email past your free allowance, and every text, is charged from it.</p>
      {b.balance <= 0 ? (
        <div className="mt-3">
          <Notice tone="amber" icon={<AlertTriangle />} title="Paid sending is paused">
            Add credit to send texts and emails beyond your free {formatNumber(b.free_emails_per_month)}.
          </Notice>
        </div>
      ) : null}
      <div className="mt-4 border-t border-zinc-100 pt-4">
        {!b.stripe_enabled ? (
          <Notice tone="gray" icon={<CreditCard />} title="Card payments aren't configured on this server">
            Contact the person who runs this DumbMail server to add credit.
          </Notice>
        ) : (
          <form onSubmit={onSubmit} className="space-y-3">
            <div className="text-xs font-medium text-zinc-700">Add credit</div>
            <div className="flex flex-wrap gap-2" role="radiogroup" aria-label="Amount">
              {presets.map((p) => (
                <AmountChip key={p} selected={preset === p} onClick={() => setPreset(p)}>
                  ${p}
                </AmountChip>
              ))}
              <AmountChip selected={preset === "custom"} onClick={() => setPreset("custom")}>
                Custom
              </AmountChip>
            </div>
            {preset === "custom" ? (
              <Field label="Amount (USD)" hint={`Between $${MIN_TOPUP} and $${formatNumber(MAX_TOPUP)}.`}>
                <Input
                  type="number"
                  min={MIN_TOPUP}
                  max={MAX_TOPUP}
                  step={1}
                  value={custom}
                  onChange={(e) => setCustom(e.target.value)}
                  placeholder="250"
                  className="w-32"
                  autoFocus
                />
              </Field>
            ) : null}
            <Button type="submit" variant="primary" disabled={!valid} loading={checkout.isPending || redirecting} icon={<CreditCard className="size-3.5" />}>
              {valid ? `Add ${formatCurrency(amount)}` : "Add credit"}
            </Button>
            <p className="text-xs text-zinc-500">You'll pay on a secure Stripe page and come straight back here.</p>
          </form>
        )}
      </div>
    </Card>
  );
}

function AmountChip({ selected, onClick, children }: { selected: boolean; onClick: () => void; children: ReactNode }) {
  return (
    <button
      type="button"
      role="radio"
      aria-checked={selected}
      onClick={onClick}
      className={cx(
        "h-8.5 min-w-16 rounded-md border px-3 text-sm font-medium tabular-nums transition-colors",
        selected ? "border-accent-600 bg-accent-50 text-accent-900 ring-1 ring-accent-600" : "border-zinc-300 bg-white text-zinc-700 hover:border-zinc-400",
      )}
    >
      {children}
    </button>
  );
}

function AutoRechargeCard({ b }: { b: Billing }) {
  const save = useAutoRecharge();
  const [draft, setDraft] = useState<AutoRecharge | null>(null);
  const current = draft ?? b.auto_recharge;
  const disabled = !b.has_payment_method;
  const edit = (patch: Partial<AutoRecharge>) => setDraft({ ...current, ...patch });
  const amountValid = current.amount >= MIN_TOPUP && current.amount <= MAX_TOPUP;
  const thresholdValid = current.threshold >= 0;

  const onSubmit = (e: FormEvent<HTMLFormElement>) => {
    e.preventDefault();
    save.mutate(current, {
      onSuccess: () => {
        setDraft(null);
        toast.success(current.enabled ? "Auto-recharge is on" : "Auto-recharge is off");
      },
    });
  };

  return (
    <Card title="Auto-recharge">
      <form onSubmit={onSubmit}>
        <fieldset disabled={disabled} className="space-y-4">
          <div className="flex items-start justify-between gap-3">
            <div className="text-sm">
              <div className="font-medium text-zinc-900">Top up automatically</div>
              <div className="text-xs text-zinc-500">So sends never stop because the balance ran out.</div>
            </div>
            <Toggle checked={current.enabled} onChange={(enabled) => edit({ enabled })} label="Auto-recharge" disabled={disabled} />
          </div>
          <div className={cx("grid gap-3 sm:grid-cols-2", !current.enabled && "opacity-60")}>
            <Field label="When the balance falls below" error={thresholdValid ? null : "Can't be negative."}>
              <DollarInput value={current.threshold} onChange={(threshold) => edit({ threshold })} min={0} />
            </Field>
            <Field label="Add this much" error={amountValid ? null : `Between $${MIN_TOPUP} and $${formatNumber(MAX_TOPUP)}.`}>
              <DollarInput value={current.amount} onChange={(amount) => edit({ amount })} min={MIN_TOPUP} max={MAX_TOPUP} />
            </Field>
          </div>
          {disabled ? (
            <p className="text-xs text-zinc-500">
              Auto-recharge needs a saved card. Add credit once with a card and it's saved for automatic top-ups.
            </p>
          ) : (
            <div className="flex justify-end">
              <Button type="submit" variant="primary" disabled={draft === null || !amountValid || !thresholdValid} loading={save.isPending}>
                Save
              </Button>
            </div>
          )}
        </fieldset>
      </form>
    </Card>
  );
}

function DollarInput({ value, onChange, min, max }: { value: number; onChange: (n: number) => void; min?: number; max?: number }) {
  return (
    <div className="relative">
      <span className="pointer-events-none absolute inset-y-0 left-2.5 flex items-center text-sm text-zinc-400">$</span>
      <Input type="number" min={min} max={max} step={1} value={value} onChange={(e) => onChange(Number(e.target.value) || 0)} className="w-full pl-6" />
    </div>
  );
}

function ThisMonthCard({ b }: { b: Billing }) {
  const freeUsed = Math.max(0, b.free_emails_per_month - b.free_emails_remaining);
  const paidEmails = Math.max(0, b.emails_this_month - freeUsed);
  const pct = b.free_emails_per_month ? Math.min(100, (freeUsed / b.free_emails_per_month) * 100) : 100;
  return (
    <Card title="This month">
      <div className="grid grid-cols-1 gap-6 sm:grid-cols-[2fr_1fr]">
        <div>
          <div className="mb-1 flex justify-between text-sm">
            <span className="text-zinc-700">Free emails</span>
            <span className="tabular-nums text-zinc-900">
              {formatNumber(freeUsed)} <span className="text-zinc-500">/ {formatNumber(b.free_emails_per_month)}</span>
            </span>
          </div>
          <div
            className="h-2 overflow-hidden rounded-full bg-zinc-100"
            role="meter"
            aria-valuenow={freeUsed}
            aria-valuemin={0}
            aria-valuemax={b.free_emails_per_month}
            aria-label="Free emails used"
          >
            <div className={cx("h-full rounded-full", pct >= 100 ? "bg-amber-500" : "bg-accent-600")} style={{ width: `${pct}%` }} />
          </div>
          <p className="mt-1.5 text-xs text-zinc-500">
            {formatNumber(b.free_emails_remaining)} free left, reset on the 1st.
            {paidEmails > 0 ? ` ${formatNumber(paidEmails)} paid emails so far.` : ` After that, ${formatPrice(b.prices.email_per_1000)} per 1,000.`}
          </p>
        </div>
        <div>
          <div className="text-sm text-zinc-700">Texts sent</div>
          <div className="text-2xl font-semibold tabular-nums text-zinc-950">{formatNumber(b.sms_this_month)}</div>
          <div className="text-xs text-zinc-500">No free allowance for SMS</div>
        </div>
      </div>
    </Card>
  );
}

/** The last 30 calendar days in the account time zone, plus any other dates the usage mentions. */
function chartDates(usageDates: string[], timeZone: string): string[] {
  const now = Date.now();
  const dates = new Set(usageDates);
  for (let i = 29; i >= 0; i--) dates.add(utcToZoned(new Date(now - i * 864e5).toISOString(), timeZone).date);
  return [...dates].sort();
}

function UsageCard({ b }: { b: Billing }) {
  const { account } = useSession();
  const [view, setView] = useState<"messages" | "spend">("messages");
  const byDate = new Map<string, { date: string; email: number; texts: number; number: number }>();
  for (const date of chartDates(b.usage.map((u) => u.date.slice(0, 10)), account.timezone)) {
    byDate.set(date, { date, email: 0, texts: 0, number: 0 });
  }
  for (const u of b.usage) {
    const row = byDate.get(u.date.slice(0, 10));
    if (!row) continue;
    const value = view === "messages" ? u.quantity : u.amount;
    if (u.kind === "email") row.email += value;
    else if (u.kind === "sms" || u.kind === "mms") row.texts += value;
    else row.number += value;
  }
  const data = [...byDate.values()];
  const series =
    view === "messages"
      ? [
          { key: "email", label: "Emails" },
          { key: "texts", label: "Texts" },
        ]
      : [
          { key: "email", label: "Emails" },
          { key: "texts", label: "Texts" },
          { key: "number", label: "Phone numbers" },
        ];
  const total = data.reduce((sum, d) => sum + d.email + d.texts + (view === "spend" ? d.number : 0), 0);

  return (
    <Card
      title="Daily usage, last 30 days"
      actions={
        <Segmented
          size="sm"
          value={view}
          onChange={setView}
          options={[
            { value: "messages", label: "Messages" },
            { value: "spend", label: "Spend" },
          ]}
        />
      }
    >
      <div className="mb-2 text-sm text-zinc-600">
        {view === "messages" ? `${formatNumber(total)} messages` : `${formatPrice(total)} spent`}
      </div>
      <TimeSeriesChart
        data={data}
        xKey="date"
        series={series}
        stacked
        format={view === "messages" ? formatNumber : (n) => formatCurrency(n, n < 10)}
        allowDecimals={view === "spend"}
        height={220}
      />
    </Card>
  );
}

function PricesCard({ b }: { b: Billing }) {
  const p = b.prices;
  // A worked example from our own prices: 10,000 contacts, 4 emails each this month.
  const exampleEmails = 40_000;
  const exampleCost = (Math.max(0, exampleEmails - b.free_emails_per_month) / 1000) * p.email_per_1000;
  const rows = [
    { item: "Email", price: `${formatPrice(p.email_per_1000)} per 1,000`, note: `First ${formatNumber(b.free_emails_per_month)} each month are free` },
    { item: "SMS", price: `${formatPrice(p.sms_per_segment)} per segment`, note: "A segment is up to 160 characters, or 70 with emoji" },
    { item: "MMS", price: `${formatPrice(p.mms_per_message)} per message`, note: "Texts with an image" },
    { item: "Phone number", price: `${formatPrice(p.phone_number_monthly)} per month`, note: "Only for numbers you get from DumbMail" },
  ];
  return (
    <Card title="Prices" padded={false}>
      <Table className="rounded-none border-0 shadow-none [&_table]:min-w-0">
        <tbody>
          {rows.map((r) => (
            <tr key={r.item} className="[&:last-child>td]:border-b-0">
              <Td className="font-medium text-zinc-900 sm:w-36">{r.item}</Td>
              <Td className="tabular-nums text-zinc-900">
                <div className="whitespace-nowrap">{r.price}</div>
                <div className="mt-0.5 text-xs text-zinc-500 sm:hidden">{r.note}</div>
              </Td>
              <Td className="hidden text-xs text-zinc-500 sm:table-cell">{r.note}</Td>
            </tr>
          ))}
        </tbody>
      </Table>
      <div className="border-t border-zinc-100 px-4 py-3 text-xs text-zinc-600">
        <p>No plans or contact tiers. Other providers charge about $150/mo just to keep 10,000 contacts; here you only pay for the messages you send.</p>
        <p className="mt-1 text-zinc-500">
          For example, 4 emails this month to each of 10,000 contacts ({formatNumber(exampleEmails)} emails) costs {formatCurrency(exampleCost, true)} here.
        </p>
      </div>
    </Card>
  );
}

function LedgerCard({ b }: { b: Billing }) {
  const { account } = useSession();
  return (
    <Card title="Transactions" padded={false}>
      <Table className="rounded-none border-0 shadow-none">
        <thead>
          <tr>
            <Th>Date</Th>
            <Th>Description</Th>
            <Th>Type</Th>
            <Th className="text-right">Amount</Th>
          </tr>
        </thead>
        <tbody>
          {b.ledger.length === 0 ? (
            <EmptyRow cols={4}>
              <p className="px-4 py-8 text-center text-sm text-zinc-500">No transactions yet. Top-ups and charges show up here.</p>
            </EmptyRow>
          ) : (
            b.ledger.map((e) => (
              <tr key={e.id} className="[&:last-child>td]:border-b-0">
                <Td className="whitespace-nowrap text-zinc-600">{formatDateTime(e.created_at, account.timezone)}</Td>
                <Td className="text-zinc-900">{e.description}</Td>
                <Td>
                  <Badge tone={e.amount > 0 ? "green" : "gray"}>{e.kind.replace(/_/g, " ")}</Badge>
                </Td>
                <Td className={cx("whitespace-nowrap text-right font-medium tabular-nums", e.amount > 0 ? "text-accent-700" : "text-zinc-900")}>
                  {e.amount > 0 ? "+" : e.amount < 0 ? "−" : ""}
                  {formatPrice(Math.abs(e.amount))}
                </Td>
              </tr>
            ))
          )}
        </tbody>
      </Table>
    </Card>
  );
}
