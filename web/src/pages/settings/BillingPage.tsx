import { useState, type FormEvent, type ReactNode } from "react";
import { redirect, type LoaderFunctionArgs } from "react-router";
import { AlertTriangle, CreditCard, Server } from "lucide-react";
import type { AutoRecharge, Billing, EmailPlan, PlanOption } from "../../api";
import { keys, queryClient, useAutoRecharge, useBilling, useCancelPlan, useCheckout, useChoosePlan, useResumePlan, useSession } from "../../api";
import { TimeSeriesChart } from "../../components/charts/TimeSeriesChart";
import {
  Badge,
  Button,
  Card,
  ConfirmDialog,
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
import { formatCurrency, formatDate, formatDateTime, formatNumber, formatPrice } from "../../lib/format";
import { toast } from "../../lib/toast";
import { utcToZoned } from "../../lib/tz";

/**
 * Stripe Checkout returns to `?topup=` or `?plan=` `success|cancelled`. Handle it before the page renders: toast,
 * refresh billing (again a few seconds later, since Stripe's webhook can land after the redirect), and drop the param.
 */
export function billingLoader({ request }: LoaderFunctionArgs) {
  const url = new URL(request.url);
  const topup = url.searchParams.get("topup");
  const plan = url.searchParams.get("plan");
  if (!topup && !plan) return null;
  if (topup === "success" || plan === "success") {
    toast.success(topup ? "Payment received. Your credit is being added." : "Payment received. Your plan is being set up.");
    void queryClient.invalidateQueries({ queryKey: keys.billing });
    setTimeout(() => void queryClient.invalidateQueries({ queryKey: keys.billing }), 4000);
  } else if (topup === "cancelled" || plan === "cancelled") {
    toast.info(`${topup ? "Top-up" : "Plan change"} cancelled. You weren't charged.`);
  }
  url.searchParams.delete("topup");
  url.searchParams.delete("plan");
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
      <PlanCard b={b} />
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

type ActivePlan = Exclude<EmailPlan, { status: "none" }>;

/** Monthly email plan: pick one, or see the current one and upgrade, downgrade or cancel it. */
function PlanCard({ b }: { b: Billing }) {
  if (!b.stripe_enabled || b.plan_options.length === 0) return null;
  return b.plan.status === "none" ? <ChoosePlan b={b} ended={b.plan} /> : <CurrentPlan b={b} plan={b.plan} />;
}

/** Sends the browser to Stripe Checkout when a plan change needs a payment now; otherwise confirms the saved change. */
function usePlanChange() {
  const choose = useChoosePlan();
  const [redirecting, setRedirecting] = useState(false);
  const change = (tier: number, saved: string) =>
    choose.mutate(tier, {
      onSuccess: ({ url }) => {
        if (url) {
          setRedirecting(true);
          window.location.assign(url);
        } else {
          toast.success(saved);
        }
      },
    });
  return { change, busy: choose.isPending || redirecting, variables: choose.variables };
}

function ChoosePlan({ b, ended }: { b: Billing; ended: Extract<EmailPlan, { status: "none" }> }) {
  const { account } = useSession();
  const { change, busy } = usePlanChange();
  const [tier, setTier] = useState(1);
  const pick = b.plan_options.find((o) => o.tier === tier) ?? b.plan_options[0];
  return (
    <Card title="Monthly email plan">
      {ended.ended_at ? (
        <div className="mb-4">
          <Notice tone="amber" icon={<AlertTriangle />} title={`Your plan ended on ${formatDate(ended.ended_at, account.timezone)}`}>
            {ended.ended_reason === "renewal_failed"
              ? "We couldn't charge your card for the renewal. Emails past your free allowance now use prepaid credit."
              : "You cancelled it. Emails past your free allowance now use prepaid credit."}
          </Notice>
        </div>
      ) : null}
      <p className="text-sm text-zinc-600">
        Pay monthly for a set number of emails at about {Math.round((1 - pick.price_per_1000 / b.prices.email_per_1000) * 100)}% less than pay-as-you-go. Each
        plan is double the one before it, so upgrading mid-month costs just the difference.
      </p>
      <div className="mt-4 grid grid-cols-2 gap-2 sm:grid-cols-3 lg:grid-cols-6" role="radiogroup" aria-label="Plan">
        {b.plan_options.map((o) => (
          <PlanChip key={o.tier} option={o} selected={o.tier === tier} onClick={() => setTier(o.tier)} />
        ))}
      </div>
      <div className="mt-4 flex flex-wrap items-center gap-3">
        <Button variant="primary" loading={busy} icon={<CreditCard className="size-3.5" />} onClick={() => change(tier, "Plan saved")}>
          Subscribe for {formatCurrency(pick.price)}/month
        </Button>
        <p className="text-xs text-zinc-500">
          Renews monthly until you cancel. Unused emails don't carry over; anything past your plan uses prepaid credit.
        </p>
      </div>
    </Card>
  );
}

function PlanChip({ option, selected, onClick }: { option: PlanOption; selected: boolean; onClick: () => void }) {
  return (
    <button
      type="button"
      role="radio"
      aria-checked={selected}
      onClick={onClick}
      className={cx(
        "rounded-md border px-3 py-2.5 text-left transition-colors",
        selected ? "border-accent-600 bg-accent-50 ring-1 ring-accent-600" : "border-zinc-300 bg-white hover:border-zinc-400",
      )}
    >
      <div className="text-sm font-semibold tabular-nums text-zinc-950">{formatCurrency(option.price)}/mo</div>
      <div className="text-xs tabular-nums text-zinc-700">{formatNumber(option.emails)} emails</div>
      <div className="mt-0.5 text-[11px] tabular-nums text-zinc-500">{formatCurrency(option.price_per_1000, true)} per 1,000</div>
    </button>
  );
}

function CurrentPlan({ b, plan }: { b: Billing; plan: ActivePlan }) {
  const { account } = useSession();
  const { change, busy, variables } = usePlanChange();
  const cancel = useCancelPlan();
  const resume = useResumePlan();
  const [confirmCancel, setConfirmCancel] = useState(false);
  const [changing, setChanging] = useState(false);
  const end = formatDate(plan.period_end, account.timezone);
  const next = b.plan_options.find((o) => o.tier === plan.tier + 1);
  const pct = plan.emails_included ? Math.min(100, (plan.emails_used / plan.emails_included) * 100) : 100;
  const badge =
    plan.status === "renewal_failed" ? (
      <Badge tone="amber">Payment failed</Badge>
    ) : plan.status === "renewing" ? (
      <Badge tone="blue">Renewing</Badge>
    ) : plan.status === "cancelling" ? (
      <Badge tone="gray">Ends {end}</Badge>
    ) : (
      <Badge tone="green">Active</Badge>
    );
  const cancelButton = (
    <Button variant="ghost" onClick={() => setConfirmCancel(true)}>
      Cancel plan
    </Button>
  );
  return (
    <Card title="Monthly email plan" actions={badge}>
      <div className="flex flex-wrap items-baseline justify-between gap-x-4 gap-y-1">
        <div className="text-lg font-semibold tabular-nums text-zinc-950">
          {formatNumber(plan.emails_included)} emails <span className="text-sm font-normal text-zinc-500">this month</span>
        </div>
        <div className="text-sm tabular-nums text-zinc-700">{formatCurrency(plan.price)}/month</div>
      </div>
      {plan.status === "renewal_failed" ? (
        <div className="mt-3 space-y-3">
          <Notice tone="amber" icon={<AlertTriangle />} title="We couldn't charge your card for this month">
            We'll try again tomorrow (try {plan.renewal_attempts} of {plan.max_renewal_attempts}). Until then, emails past your free allowance use prepaid
            credit. Paying with another card starts a new month right away.
          </Notice>
          <div className="flex flex-wrap gap-2">
            <Button variant="primary" loading={busy} onClick={() => change(plan.tier, "Plan saved")}>
              Pay {formatCurrency(plan.price)} with another card
            </Button>
            {cancelButton}
          </div>
        </div>
      ) : plan.status === "renewing" ? (
        <div className="mt-3 space-y-3">
          <p className="text-sm text-zinc-700">
            Your month ended on {end}. We're charging your card {formatCurrency(plan.price)} for the next one; this usually takes a minute.
          </p>
          <div className="flex flex-wrap gap-2">{cancelButton}</div>
        </div>
      ) : (
        <>
          <div
            className="mt-3 h-2 overflow-hidden rounded-full bg-zinc-100"
            role="meter"
            aria-valuenow={plan.emails_used}
            aria-valuemin={0}
            aria-valuemax={plan.emails_included}
            aria-label="Plan emails used"
          >
            <div className={cx("h-full rounded-full", pct >= 90 ? "bg-amber-500" : "bg-accent-600")} style={{ width: `${pct}%` }} />
          </div>
          <p className="mt-1.5 text-xs tabular-nums text-zinc-500">
            {formatNumber(plan.emails_used)} used · {formatNumber(plan.emails_remaining)} left until {end}
          </p>
          <p className="mt-3 text-sm text-zinc-700">
            {plan.status === "cancelling"
              ? `Your plan ends on ${end} and won't renew. After that, emails past your free allowance use prepaid credit.`
              : plan.next_tier && plan.next_emails && plan.next_price
                ? `On ${end} it changes to ${formatNumber(plan.next_emails)} emails for ${formatCurrency(plan.next_price)}/month.`
                : `Renews on ${end} for ${formatCurrency(plan.price)}.`}
          </p>
          <div className="mt-4 flex flex-wrap gap-2">
            {plan.status === "cancelling" ? (
              <Button variant="primary" loading={resume.isPending} onClick={() => resume.mutate(undefined, { onSuccess: () => toast.success("Your plan will renew") })}>
                Keep my plan
              </Button>
            ) : (
              <>
                {next ? (
                  <Button variant="primary" loading={busy && variables === next.tier} onClick={() => change(next.tier, "Plan saved")}>
                    Double to {formatNumber(next.emails)} emails for {formatCurrency(next.price - plan.price)} more
                  </Button>
                ) : null}
                {plan.next_tier ? (
                  <Button loading={busy && variables === plan.tier} onClick={() => change(plan.tier, "You'll stay on your current plan")}>
                    Keep {formatNumber(plan.emails_included)} emails
                  </Button>
                ) : null}
                <Button onClick={() => setChanging((v) => !v)} aria-expanded={changing}>
                  Change plan
                </Button>
                {cancelButton}
              </>
            )}
          </div>
          {changing && plan.status !== "cancelling" ? (
            <div className="mt-4 border-t border-zinc-100 pt-4">
              <p className="mb-2 text-xs text-zinc-500">
                A bigger plan starts now: you pay the difference today. A smaller one starts at your next renewal on {end}.
              </p>
              <div className="grid grid-cols-2 gap-2 sm:grid-cols-3 lg:grid-cols-6" role="radiogroup" aria-label="Change plan to">
                {b.plan_options.map((o) => (
                  <PlanChip
                    key={o.tier}
                    option={o}
                    selected={o.tier === (plan.next_tier ?? plan.tier)}
                    onClick={() =>
                      change(o.tier, o.tier < plan.tier ? `Your plan changes on ${end}` : "You'll stay on your current plan")
                    }
                  />
                ))}
              </div>
            </div>
          ) : null}
        </>
      )}
      {confirmCancel ? (
        <ConfirmDialog
          title="Cancel your email plan?"
          body={
            plan.status === "active"
              ? `It stays active until ${end} and won't renew. After that, emails past your free allowance use prepaid credit.`
              : "It ends now and won't be charged again. Emails past your free allowance use prepaid credit."
          }
          confirmLabel="Cancel plan"
          danger
          loading={cancel.isPending}
          onConfirm={() =>
            cancel.mutate(undefined, {
              onSuccess: () => {
                setConfirmCancel(false);
                toast.success(plan.status === "active" ? `Plan cancelled. It stays active until ${end}.` : "Plan cancelled.");
              },
            })
          }
          onClose={() => setConfirmCancel(false)}
        />
      ) : null}
    </Card>
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

// Stripe's fixed 30¢ per payment makes small top-ups costly, so the smallest is $25 and $50 is suggested.
const presets = [25, 50, 100, 250] as const;
const MAX_TOPUP = 5000;

function BalanceCard({ b }: { b: Billing }) {
  const checkout = useCheckout();
  const [preset, setPreset] = useState<number | "custom">(50);
  const [custom, setCustom] = useState("");
  const [redirecting, setRedirecting] = useState(false);
  const MIN_TOPUP = b.min_topup;
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
      <p className="mt-1 text-xs text-zinc-500">
        Prepaid credit. Every text, and each email past your free allowance{b.plan.status !== "none" ? " and your plan" : ""}, is charged from it.
      </p>
      {b.balance <= 0 ? (
        <div className="mt-3">
          {b.plan.status === "active" || b.plan.status === "cancelling" ? (
            <Notice tone="gray" icon={<AlertTriangle />} title="No credit for texts">
              Emails come from your free allowance and your plan. Add credit to send texts, or emails once your plan runs out.
            </Notice>
          ) : (
            <Notice tone="amber" icon={<AlertTriangle />} title="Paid sending is paused">
              Add credit to send texts and emails beyond your free {formatNumber(b.free_emails_per_month)}.
            </Notice>
          )}
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
  const MIN_TOPUP = b.min_topup;
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
            {b.plan.status === "active" || b.plan.status === "cancelling"
              ? " After that, emails come from your plan."
              : paidEmails > 0
                ? ` ${formatNumber(paidEmails)} paid emails so far.`
                : ` After that, ${formatPrice(b.prices.email_per_1000)} per 1,000.`}
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
  const smallest = b.plan_options[0];
  const rows = [
    { item: "Email", price: `${formatPrice(p.email_per_1000)} per 1,000`, note: `First ${formatNumber(b.free_emails_per_month)} each month are free` },
    ...(smallest
      ? [
          {
            item: "Email plan",
            price: `From ${formatCurrency(smallest.price)} a month`,
            note: `${formatNumber(smallest.emails)} emails (${formatCurrency(smallest.price_per_1000, true)} per 1,000); each larger plan doubles`,
          },
        ]
      : []),
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
        <p>No contact tiers. Other providers charge about $150/mo just to keep 10,000 contacts; here you pay only for the messages you send, as you go or with a monthly plan.</p>
        <p className="mt-1 text-zinc-500">
          For example, 4 emails this month to each of 10,000 contacts ({formatNumber(exampleEmails)} emails) costs {formatCurrency(exampleCost, true)} here.
        </p>
      </div>
    </Card>
  );
}

type LedgerRow = { id: string; kind: string; description: string; amount: number; created_at: string; card: boolean };

function LedgerCard({ b }: { b: Billing }) {
  const { account } = useSession();
  // Credit moves and plan card payments in one list, newest first. Plan payments go on the card, not the balance.
  const rows: LedgerRow[] = [
    ...b.ledger.map((e) => ({ ...e, card: false })),
    ...b.plan_payments.map((p) => ({ id: p.id, kind: "plan", description: p.description, amount: p.amount, created_at: p.created_at, card: true })),
  ].sort((x, y) => y.created_at.localeCompare(x.created_at));
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
          {rows.length === 0 ? (
            <EmptyRow cols={4}>
              <p className="px-4 py-8 text-center text-sm text-zinc-500">No transactions yet. Top-ups, plan payments and charges show up here.</p>
            </EmptyRow>
          ) : (
            rows.map((e) => (
              <tr key={e.id} className="[&:last-child>td]:border-b-0">
                <Td className="whitespace-nowrap text-zinc-600">{formatDateTime(e.created_at, account.timezone)}</Td>
                <Td className="text-zinc-900">{e.description}</Td>
                <Td>
                  <Badge tone={e.card ? "blue" : e.amount > 0 ? "green" : "gray"}>{e.kind.replace(/_/g, " ")}</Badge>
                </Td>
                {e.card ? (
                  <Td className="whitespace-nowrap text-right font-medium tabular-nums text-zinc-900">
                    {formatCurrency(e.amount, true)} <span className="text-xs font-normal text-zinc-500">card</span>
                  </Td>
                ) : (
                  <Td className={cx("whitespace-nowrap text-right font-medium tabular-nums", e.amount > 0 ? "text-accent-700" : "text-zinc-900")}>
                    {e.amount > 0 ? "+" : e.amount < 0 ? "−" : ""}
                    {formatPrice(Math.abs(e.amount))}
                  </Td>
                )}
              </tr>
            ))
          )}
        </tbody>
      </Table>
    </Card>
  );
}
