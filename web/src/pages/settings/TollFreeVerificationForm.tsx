import { useState, type FormEvent, type ReactNode } from "react";
import { ExternalLink } from "lucide-react";
import type { Account, Form, MonthlyVolume, PhoneNumber, TollFreeUseCase, TollFreeVerification, User } from "../../api";
import { errorMessage, useSubmitVerification } from "../../api";
import { Button, Field, InlineError, Input, Modal, Select, Textarea } from "../../components/ui";
import { formatPhone } from "../../lib/format";
import { toast } from "../../lib/toast";
import { hostnameOf } from "./shared";
import { hasPhoneField } from "../forms/formConfig";

const useCases: TollFreeUseCase[] = ["Marketing", "Mixed", "Account Notifications", "Customer Care", "Delivery Notifications"];
const volumes: MonthlyVolume[] = ["10", "100", "1,000", "10,000", "100,000"];

type Address = Pick<TollFreeVerification, "address1" | "address2" | "city" | "state" | "zip">;

/**
 * Best-effort split of the account's free-text mailing address ("123 Main St, Suite 4, Portland, OR 97201",
 * commas or new lines) into the fields carriers ask for. Anything it can't place stays in address1.
 */
export function parseAddress(raw: string | null): Address {
  const out: Address = { address1: "", address2: "", city: "", state: "", zip: "" };
  if (!raw) return out;
  const parts = raw
    .split(/[\n,]+/)
    .map((s) => s.trim())
    .filter(Boolean);
  if (/^(us|usa|united states( of america)?)$/i.test(parts[parts.length - 1] ?? "")) parts.pop();
  const m = /^(?:(.*?)\s+)?([A-Za-z]{2})\.?\s+(\d{5}(?:-\d{4})?)$/.exec(parts[parts.length - 1] ?? "");
  if (m) {
    out.state = m[2].toUpperCase();
    out.zip = m[3];
    parts.pop();
    if (m[1]) parts.push(m[1]);
  }
  if (parts.length >= 2) out.city = parts.pop() ?? "";
  out.address1 = parts.shift() ?? "";
  out.address2 = parts.join(", ");
  return out;
}

/** Carriers want to see the opt-in. A live form that collects phone numbers is the best evidence; any live form will do. */
function optInFormUrl(forms: Form[], origin: string): string {
  const live = forms.filter((f) => f.status === "live");
  const best = live.find((f) => hasPhoneField(f.config) || Boolean(f.config.phone_step?.enabled)) ?? live[0];
  return best ? `${origin}/f/${best.id}` : "";
}

export function verificationDefaults(account: Account, user: User, forms: Form[], origin: string): TollFreeVerification {
  const brand = account.name;
  const site = hostnameOf(account.website);
  const [first = "", ...rest] = (user.name ?? "").trim().split(/\s+/);
  const optInUrl = optInFormUrl(forms, origin);
  return {
    business_name: brand,
    website: account.website ?? "",
    ...parseAddress(account.address),
    contact_first_name: first,
    contact_last_name: rest.join(" "),
    contact_email: user.email,
    contact_phone: "",
    use_case: "Marketing",
    use_case_summary: `${brand} sends recurring marketing texts about new products, sales and promotions to customers who signed up for texts on our website. Message frequency varies.`,
    sample_message: `${brand}: Our weekend sale is on! Take 20% off everything through Sunday with code WEEKEND20${site ? ` at ${site}` : ""}. Reply STOP to opt out.`,
    opt_in_description: `Customers opt in by entering their phone number in the sign-up form on our website${optInUrl ? ` (${optInUrl})` : ""}. The consent text next to the field says they agree to receive recurring automated marketing texts, that consent is not a condition of purchase, that msg & data rates may apply, and that they can reply STOP to opt out.`,
    opt_in_image_url: optInUrl,
    monthly_volume: "1,000",
  };
}

type Props = { number: PhoneNumber; initial: TollFreeVerification; onClose: () => void };

export function TollFreeVerificationForm({ number, initial, onClose }: Props) {
  const submit = useSubmitVerification();
  const [v, setV] = useState<TollFreeVerification>(initial);
  const set = <K extends keyof TollFreeVerification>(key: K, value: TollFreeVerification[K]) => setV((old) => ({ ...old, [key]: value }));
  const text = (key: Exclude<keyof TollFreeVerification, "use_case" | "monthly_volume">) => ({
    value: v[key] ?? "",
    onChange: (e: { target: { value: string } }) => set(key, e.target.value),
  });

  const onSubmit = (e: FormEvent<HTMLFormElement>) => {
    e.preventDefault();
    const input: TollFreeVerification = { ...v, address2: v.address2?.trim() || undefined };
    submit.mutate(
      { id: number.id, input },
      {
        onSuccess: () => {
          toast.success("Verification submitted. Carriers usually reply in 1–5 business days.");
          onClose();
        },
      },
    );
  };

  return (
    <Modal
      title={`Verify ${formatPhone(number.phone_number)} for texting`}
      description="Carriers review every toll-free number that sends marketing texts. It's free and usually takes 1–5 business days."
      onClose={onClose}
      size="lg"
    >
      <form onSubmit={onSubmit} className="space-y-5">
        {number.status === "rejected" && number.verification_note ? (
          <InlineError message={`Carrier feedback: ${number.verification_note}`} />
        ) : null}
        <Section title="Business">
          <Field label="Legal business name">
            <Input required {...text("business_name")} />
          </Field>
          <Field label="Website">
            <Input required type="url" placeholder="https://" {...text("website")} />
          </Field>
          <Field label="Street address" className="sm:col-span-2">
            <Input required {...text("address1")} />
          </Field>
          <Field label="Address line 2 (optional)" className="sm:col-span-2">
            <Input {...text("address2")} />
          </Field>
          <Field label="City">
            <Input required {...text("city")} />
          </Field>
          <div className="grid grid-cols-2 gap-3">
            <Field label="State">
              <Input required maxLength={2} placeholder="OR" {...text("state")} />
            </Field>
            <Field label="ZIP">
              <Input required inputMode="numeric" {...text("zip")} />
            </Field>
          </div>
        </Section>
        <Section title="Contact" hint="Who carriers reach with questions about this number.">
          <Field label="First name">
            <Input required {...text("contact_first_name")} />
          </Field>
          <Field label="Last name">
            <Input required {...text("contact_last_name")} />
          </Field>
          <Field label="Email">
            <Input required type="email" {...text("contact_email")} />
          </Field>
          <Field label="Phone">
            <Input required type="tel" placeholder="+15035550100" {...text("contact_phone")} />
          </Field>
        </Section>
        <Section title="Messages">
          <Field label="Use case">
            <Select value={v.use_case} onChange={(e) => set("use_case", e.target.value as TollFreeUseCase)}>
              {useCases.map((u) => (
                <option key={u}>{u}</option>
              ))}
            </Select>
          </Field>
          <Field label="Texts per month">
            <Select value={v.monthly_volume} onChange={(e) => set("monthly_volume", e.target.value as MonthlyVolume)}>
              {volumes.map((n) => (
                <option key={n} value={n}>
                  Up to {n}
                </option>
              ))}
            </Select>
          </Field>
          <Field label="What you send and to whom" className="sm:col-span-2">
            <Textarea required rows={2} {...text("use_case_summary")} />
          </Field>
          <Field
            label="Sample message"
            className="sm:col-span-2"
            hint="A realistic text with your brand name and opt-out language. Carriers reject samples without them."
          >
            <Textarea required rows={2} {...text("sample_message")} />
          </Field>
        </Section>
        <Section title="Opt-in">
          <Field label="How people sign up for texts" className="sm:col-span-2">
            <Textarea required rows={3} {...text("opt_in_description")} />
          </Field>
          <Field
            label="Link to the opt-in"
            className="sm:col-span-2"
            hint={
              <>
                {v.opt_in_image_url
                  ? "A page or screenshot showing the sign-up form and its consent text. Your hosted form page works."
                  : "A page or screenshot showing the sign-up form and its consent text. No sign-up form is live yet: publish one that collects phone numbers and use its hosted page link."}
                {v.opt_in_image_url ? (
                  <>
                    {" "}
                    <a href={v.opt_in_image_url} target="_blank" rel="noreferrer" className="inline-flex items-center gap-0.5 font-medium text-accent-700 hover:underline">
                      Open <ExternalLink className="size-3" />
                    </a>
                  </>
                ) : null}
              </>
            }
          >
            <Input required type="url" placeholder="https://" {...text("opt_in_image_url")} />
          </Field>
        </Section>
        <InlineError message={submit.error ? errorMessage(submit.error) : null} />
        <div className="flex justify-end gap-2 border-t border-zinc-100 pt-4">
          <Button variant="ghost" onClick={onClose}>
            Cancel
          </Button>
          <Button type="submit" variant="primary" loading={submit.isPending}>
            Submit verification
          </Button>
        </div>
      </form>
    </Modal>
  );
}

function Section({ title, hint, children }: { title: string; hint?: string; children: ReactNode }) {
  return (
    <fieldset>
      <legend className="text-xs font-semibold uppercase tracking-wider text-zinc-400">{title}</legend>
      {hint ? <p className="mt-0.5 text-xs text-zinc-500">{hint}</p> : null}
      <div className="mt-2 grid grid-cols-1 gap-3 sm:grid-cols-2">{children}</div>
    </fieldset>
  );
}
