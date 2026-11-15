import { useState, type FormEvent } from "react";
import { Link } from "react-router";
import { AlertTriangle, CheckCircle2, Clock, Info, Phone, Search, Trash2, XCircle } from "lucide-react";
import type { AvailableNumber, AvailableQuery, PhoneNumber, PhoneNumberStatus, PhoneNumberType, SmsConnectInput, SmsProvider, SmsSetup } from "../../api";
import {
  errorMessage,
  useAvailableNumbers,
  useClaimNumber,
  useConnectSms,
  useDisconnectSms,
  useForms,
  usePhoneNumbers,
  useReleaseNumber,
  useSession,
} from "../../api";
import {
  Badge,
  Button,
  Card,
  ConfirmDialog,
  CopyField,
  Field,
  InlineError,
  Input,
  Notice,
  Segmented,
  Skeleton,
  StatusBadge,
} from "../../components/ui";
import { formatDate, formatPhone, formatPrice } from "../../lib/format";
import { toast } from "../../lib/toast";
import { TollFreeVerificationForm, verificationDefaults } from "./TollFreeVerificationForm";

const statusLabels: Record<PhoneNumberStatus, string> = {
  active: "Active",
  needs_verification: "Needs verification",
  verification_pending: "In review",
  verified: "Verified",
  rejected: "Rejected",
  released: "Released",
};

const typeLabels: Record<PhoneNumberType, string> = { toll_free: "Toll-free", local: "Local" };
const providerLabels: Record<string, string> = { telnyx: "Telnyx", twilio: "Twilio", log: "Log only" };

export function SmsPage() {
  const numbers = usePhoneNumbers();
  if (numbers.isPending) {
    return (
      <div className="space-y-4">
        <Skeleton className="h-14" />
        <Skeleton className="h-64" />
        <Skeleton className="h-48" />
      </div>
    );
  }
  if (numbers.error) return <p className="text-sm text-red-600">{numbers.error.message}</p>;
  const { data, sms } = numbers.data;
  const purchased = data.filter((n) => n.source === "purchased" && n.status !== "released");
  const connected = data.find((n) => n.source === "connected" && n.status !== "released") ?? null;
  return (
    <div className="space-y-4">
      <StatusBanner sms={sms} purchased={purchased} />
      {sms.platform_available || purchased.length > 0 ? <PlatformCard sms={sms} purchased={purchased} /> : null}
      <ByoCard sms={sms} connected={connected} />
    </div>
  );
}

function StatusBanner({ sms, purchased }: { sms: SmsSetup; purchased: PhoneNumber[] }) {
  const { account } = useSession();
  const pastDue = purchased.find((n) => n.release_at);
  if (pastDue?.release_at) {
    return (
      <Notice tone="red" icon={<AlertTriangle />} title={`Add credit to keep ${formatPhone(pastDue.phone_number)}`}>
        Your balance couldn't cover this month's {formatPrice(pastDue.monthly_price)} for this number. Add credit in{" "}
        <Link to="/settings/billing" className="font-medium underline">
          Billing
        </Link>{" "}
        by {formatDate(pastDue.release_at, account.timezone)} or the number is released and can't be recovered.
      </Notice>
    );
  }
  if (sms.can_send) {
    return (
      <div className="space-y-2">
        <Notice tone="green" icon={<CheckCircle2 />} title="Ready to send texts">
          From {formatPhone(sms.from_number)} via {providerLabels[sms.provider] ?? sms.provider}
          {sms.mode === "byo" ? " (your own account)" : ""}.
        </Notice>
        {sms.provider === "log" ? (
          <Notice tone="blue" icon={<Info />} title="This server only logs texts">
            No SMS provider is configured on the server, so texts are written to the log instead of being sent.
          </Notice>
        ) : null}
      </div>
    );
  }
  const rejected = purchased.find((n) => n.status === "rejected");
  const needs = purchased.find((n) => n.status === "needs_verification");
  const pending = purchased.find((n) => n.status === "verification_pending");
  if (rejected) {
    return (
      <Notice tone="red" icon={<XCircle />} title={`Carriers rejected the verification for ${formatPhone(rejected.phone_number)}`}>
        {rejected.verification_note ? `${rejected.verification_note} ` : ""}Fix the details and resubmit below.
      </Notice>
    );
  }
  if (needs) {
    return (
      <Notice tone="amber" icon={<AlertTriangle />} title="Verify your toll-free number to start texting">
        Carriers require a free toll-free verification before {formatPhone(needs.phone_number)} can send. It usually takes 1–5 business days.
      </Notice>
    );
  }
  if (pending) {
    return (
      <Notice tone="blue" icon={<Clock />} title="Carriers are reviewing your verification">
        This usually takes 1–5 business days. {formatPhone(pending.phone_number)} can send as soon as it's approved.
      </Notice>
    );
  }
  return (
    <Notice tone="amber" icon={<Phone />} title="You can't send texts yet">
      {sms.platform_available
        ? "Get a number from DumbMail below, or connect your own Telnyx or Twilio account."
        : "Connect your own Telnyx or Twilio account below to send texts."}
    </Notice>
  );
}

// ---- Numbers from DumbMail ----

function PlatformCard({ sms, purchased }: { sms: SmsSetup; purchased: PhoneNumber[] }) {
  const { account, user } = useSession();
  const forms = useForms();
  const release = useReleaseNumber();
  const [verifying, setVerifying] = useState<PhoneNumber | null>(null);
  const [releasing, setReleasing] = useState<PhoneNumber | null>(null);

  return (
    <Card title="Get a number from DumbMail">
      <div className="space-y-4">
        {purchased.length > 0 ? (
          <ul className="divide-y divide-zinc-100 rounded-lg border border-zinc-200">
            {purchased.map((n) => (
              <li key={n.id} className="flex flex-wrap items-center gap-3 px-3 py-2.5">
                <div className="min-w-0 flex-1">
                  <div className="flex flex-wrap items-center gap-2">
                    <span className="font-medium tabular-nums text-zinc-900">{formatPhone(n.phone_number)}</span>
                    <Badge>{typeLabels[n.type]}</Badge>
                    <StatusBadge status={n.status} label={statusLabels[n.status]} />
                    {sms.from_number === n.phone_number ? <Badge tone="violet">Sending number</Badge> : null}
                  </div>
                  <div className="mt-0.5 text-xs text-zinc-500">
                    {formatPrice(n.monthly_price)}/month · since {formatDate(n.created_at, account.timezone)}
                  </div>
                  {n.status === "rejected" && n.verification_note ? (
                    <div className="mt-1 text-xs text-red-700">Carrier feedback: {n.verification_note}</div>
                  ) : null}
                </div>
                <div className="flex gap-2">
                  {n.status === "needs_verification" || n.status === "rejected" ? (
                    <Button size="sm" variant="primary" onClick={() => setVerifying(n)}>
                      {n.status === "rejected" ? "Fix and resubmit" : "Verify for texting"}
                    </Button>
                  ) : null}
                  <Button size="sm" variant="ghost" icon={<Trash2 className="size-3.5" />} onClick={() => setReleasing(n)}>
                    Release
                  </Button>
                </div>
              </li>
            ))}
          </ul>
        ) : null}
        {sms.mode === "byo" ? (
          <p className="text-sm text-zinc-500">You're sending through your own provider. Disconnect it below to send from a DumbMail number.</p>
        ) : sms.platform_available ? (
          <NumberSearch hasNumbers={purchased.length > 0} onClaimed={(n) => (n.status === "needs_verification" ? setVerifying(n) : undefined)} />
        ) : null}
      </div>
      {verifying ? (
        <TollFreeVerificationForm
          number={verifying}
          initial={verificationDefaults(account, user, forms.data ?? [], window.location.origin)}
          onClose={() => setVerifying(null)}
        />
      ) : null}
      {releasing ? (
        <ConfirmDialog
          title={`Release ${formatPhone(releasing.phone_number)}?`}
          body="You stop paying for it and can't send from it any more. Released numbers usually can't be claimed back."
          confirmLabel="Release number"
          danger
          loading={release.isPending}
          onConfirm={() =>
            release.mutate(releasing.id, {
              onSuccess: () => {
                toast.success("Number released");
                setReleasing(null);
              },
            })
          }
          onClose={() => setReleasing(null)}
        />
      ) : null}
    </Card>
  );
}

function NumberSearch({ hasNumbers, onClaimed }: { hasNumbers: boolean; onClaimed: (n: PhoneNumber) => void }) {
  const [type, setType] = useState<PhoneNumberType>("toll_free");
  const [areaCode, setAreaCode] = useState("");
  const [query, setQuery] = useState<AvailableQuery | null>(null);
  const [claiming, setClaiming] = useState<AvailableNumber | null>(null);
  const available = useAvailableNumbers(query);
  const claim = useClaimNumber();

  const onSearch = (e: FormEvent<HTMLFormElement>) => {
    e.preventDefault();
    setQuery(type === "local" ? { type, area_code: areaCode.trim() || undefined } : { type });
  };

  return (
    <div className="space-y-3">
      {hasNumbers ? <div className="text-xs font-semibold uppercase tracking-wider text-zinc-400">Add another number</div> : null}
      <Segmented
        value={type}
        onChange={(t) => {
          setType(t);
          setQuery(null);
        }}
        options={[
          { value: "toll_free", label: "Toll-free (recommended)" },
          { value: "local", label: "Local" },
        ]}
      />
      <p className="text-sm text-zinc-600">
        {type === "toll_free"
          ? "The quickest way to send marketing texts in the US and Canada. Carriers require a toll-free verification, which is free and usually takes 1–5 business days. Claim the number now and submit it right away."
          : "A number with an area code your customers know. US carriers require extra brand registration for marketing texts from local numbers, so toll-free is usually faster to get going."}
      </p>
      <form onSubmit={onSearch} className="flex flex-wrap items-end gap-2">
        {type === "local" ? (
          <Field label="Area code">
            <Input
              value={areaCode}
              onChange={(e) => setAreaCode(e.target.value.replace(/\D/g, "").slice(0, 3))}
              inputMode="numeric"
              placeholder="503"
              className="w-24"
            />
          </Field>
        ) : null}
        <Button type="submit" icon={<Search className="size-3.5" />} loading={available.isFetching}>
          {type === "toll_free" ? "Find toll-free numbers" : "Search"}
        </Button>
      </form>
      {available.error ? <InlineError message={errorMessage(available.error)} /> : null}
      {available.data ? (
        available.data.length === 0 ? (
          <p className="text-sm text-zinc-500">No numbers available{query?.area_code ? ` in ${query.area_code}` : ""}. Try another area code.</p>
        ) : (
          <ul className="divide-y divide-zinc-100 rounded-lg border border-zinc-200">
            {available.data.map((n) => (
              <li key={n.phone_number} className="flex items-center gap-3 px-3 py-2">
                <span className="min-w-0 flex-1">
                  <span className="block font-medium tabular-nums text-zinc-900">{formatPhone(n.phone_number)}</span>
                  <span className="block text-xs text-zinc-500">
                    {[typeLabels[n.type], n.region].filter(Boolean).join(" · ")} · {formatPrice(n.monthly_price)}/month
                  </span>
                </span>
                <Button size="sm" variant="primary" onClick={() => setClaiming(n)}>
                  Claim
                </Button>
              </li>
            ))}
          </ul>
        )
      ) : null}
      {claiming ? (
        <ConfirmDialog
          title={`Claim ${formatPhone(claiming.phone_number)}?`}
          body={
            <>
              It costs <strong className="text-zinc-900">{formatPrice(claiming.monthly_price)} per month</strong>, charged from your credit now and every
              month until you release it. It becomes your sending number.
              {claiming.type === "toll_free" ? " Next, you'll submit the free toll-free verification." : ""}
            </>
          }
          confirmLabel={`Claim for ${formatPrice(claiming.monthly_price)}/mo`}
          loading={claim.isPending}
          onConfirm={() =>
            claim.mutate(claiming.phone_number, {
              onSuccess: (number) => {
                toast.success(`${formatPhone(number.phone_number)} is yours`);
                setClaiming(null);
                setQuery(null);
                onClaimed(number);
              },
            })
          }
          onClose={() => setClaiming(null)}
        />
      ) : null}
    </div>
  );
}

// ---- Bring your own provider ----

const webhookSteps: Record<SmsProvider, string> = {
  telnyx: "In the Telnyx portal, open Messaging, choose the Messaging profile your number uses, paste this into Inbound → Webhook URL and save.",
  twilio:
    "In the Twilio console, open Phone Numbers → Active numbers and pick your number. Under Messaging, set \"A message comes in\" to Webhook, paste this URL, keep HTTP POST and save.",
};

function ByoCard({ sms, connected }: { sms: SmsSetup; connected: PhoneNumber | null }) {
  const disconnect = useDisconnectSms();
  const [confirm, setConfirm] = useState(false);
  const provider: SmsProvider | null = connected?.provider ?? (sms.provider === "telnyx" || sms.provider === "twilio" ? sms.provider : null);

  return (
    <Card title="Use your own Telnyx or Twilio" actions={sms.mode === "byo" ? <Badge tone="green" dot>Connected</Badge> : null}>
      {sms.mode === "byo" ? (
        <div className="space-y-4">
          <div className="grid grid-cols-2 gap-3 rounded-lg bg-zinc-50 p-3 text-sm">
            <div>
              <div className="text-xs text-zinc-500">Provider</div>
              <div className="font-medium text-zinc-900">{provider ? providerLabels[provider] : sms.provider}</div>
            </div>
            <div>
              <div className="text-xs text-zinc-500">Number</div>
              <div className="font-medium tabular-nums text-zinc-900">{formatPhone(connected?.phone_number ?? sms.from_number)}</div>
            </div>
          </div>
          <Field label="Inbound webhook URL" hint="So STOP replies unsubscribe people. Required by carriers.">
            <CopyField value={sms.inbound_webhook_url} />
          </Field>
          {provider ? <p className="text-sm text-zinc-600">{webhookSteps[provider]}</p> : null}
          <div className="flex justify-end">
            <Button variant="danger" onClick={() => setConfirm(true)}>
              Disconnect
            </Button>
          </div>
          {confirm ? (
            <ConfirmDialog
              title={`Disconnect ${provider ? providerLabels[provider] : "your provider"}?`}
              body="DumbMail deletes the saved credentials and stops sending through your account. Texts go back to your DumbMail number, if you have one."
              confirmLabel="Disconnect"
              danger
              loading={disconnect.isPending}
              onConfirm={() =>
                disconnect.mutate(undefined, {
                  onSuccess: () => {
                    toast.success("Provider disconnected");
                    setConfirm(false);
                  },
                })
              }
              onClose={() => setConfirm(false)}
            />
          ) : null}
        </div>
      ) : (
        <ConnectForm />
      )}
    </Card>
  );
}

function ConnectForm() {
  const connect = useConnectSms();
  const [provider, setProvider] = useState<SmsProvider>("telnyx");

  const onSubmit = (e: FormEvent<HTMLFormElement>) => {
    e.preventDefault();
    const d = new FormData(e.currentTarget);
    const get = (k: string) => String(d.get(k) ?? "").trim();
    const phone_number = get("phone_number");
    const input: SmsConnectInput =
      provider === "telnyx"
        ? { provider, api_key: get("api_key"), phone_number, messaging_profile_id: get("messaging_profile_id") || undefined }
        : { provider, account_sid: get("account_sid"), auth_token: get("auth_token"), phone_number };
    connect.mutate(input, { onSuccess: () => toast.success("Connected. Add the inbound webhook URL below.") });
  };

  return (
    <form onSubmit={onSubmit} className="space-y-4">
      <p className="text-sm text-zinc-600">
        Already have an account and number? DumbMail sends through it and adds no fees; you pay your provider directly. Credentials are checked
        with the provider before they're saved, and stored encrypted.
      </p>
      <Segmented
        value={provider}
        onChange={(p) => {
          setProvider(p);
          connect.reset();
        }}
        options={[
          { value: "telnyx", label: "Telnyx" },
          { value: "twilio", label: "Twilio" },
        ]}
      />
      {/* key: the two providers' fields are different inputs, so switching clears them. */}
      <div key={provider} className="grid grid-cols-1 gap-4 sm:grid-cols-2">
        {provider === "telnyx" ? (
          <>
            <Field label="API key" className="sm:col-span-2" hint="Telnyx portal → Account settings → Keys & credentials.">
              <Input name="api_key" type="password" required autoComplete="off" placeholder="KEY0…" />
            </Field>
            <Field label="Messaging profile ID (optional)" hint="Uses the number's current profile if empty.">
              <Input name="messaging_profile_id" autoComplete="off" />
            </Field>
          </>
        ) : (
          <>
            <Field label="Account SID" hint="Twilio console home page.">
              <Input name="account_sid" required autoComplete="off" placeholder="AC…" />
            </Field>
            <Field label="Auth token">
              <Input name="auth_token" type="password" required autoComplete="off" />
            </Field>
          </>
        )}
        <Field label="Phone number" hint="E.164, e.g. +15035550100. Must be on this account.">
          <Input name="phone_number" type="tel" required placeholder="+15035550100" pattern="\+[1-9]\d{6,14}" />
        </Field>
      </div>
      <InlineError message={connect.error ? errorMessage(connect.error) : null} />
      <div className="flex justify-end">
        <Button type="submit" variant="primary" loading={connect.isPending}>
          Connect {provider === "telnyx" ? "Telnyx" : "Twilio"}
        </Button>
      </div>
    </form>
  );
}
