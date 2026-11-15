import { useState, type FormEvent, type ReactNode } from "react";
import { useNavigate } from "react-router";
import { Check, FileUp, Sparkles } from "lucide-react";
import type { ImportResult } from "../../api";
import { ChannelIcons, blueprintChannels } from "../../components/flows/channels";
import {
  useBlueprints,
  useCreateFlowFromBlueprint,
  useImportProfiles,
  useSession,
  useSetFlowStatus,
  useUpdateAccount,
} from "../../api";
import { Logo } from "../../components/shell/Logo";
import { Button, ColorInput, Field, InlineError, Input, Skeleton, Textarea, cx } from "../../components/ui";
import { formatNumber } from "../../lib/format";

const steps = ["Brand", "Contacts", "Starter flow"] as const;

export function OnboardingPage() {
  const { account } = useSession();
  const navigate = useNavigate();
  const update = useUpdateAccount();
  const [step, setStep] = useState(() => {
    const o = account.onboarding;
    if (!o.brand) return 0;
    if (!o.contacts) return 1;
    return 2;
  });

  const next = () => (step < steps.length - 1 ? setStep(step + 1) : navigate("/"));

  return (
    <div className="min-h-dvh bg-zinc-50">
      <header className="mx-auto flex max-w-3xl items-center justify-between px-4 py-5">
        <Logo />
        <button type="button" className="text-sm text-zinc-500 hover:text-zinc-900" onClick={() => navigate("/")}>
          Skip setup
        </button>
      </header>
      <div className="mx-auto max-w-3xl px-4 pb-16">
        <div className="mb-6">
          <div className="mb-2 flex justify-between text-xs font-medium text-zinc-500">
            {steps.map((s, i) => (
              <button
                key={s}
                type="button"
                onClick={() => setStep(i)}
                className={cx("flex items-center gap-1.5", i === step ? "text-zinc-900" : i < step ? "text-accent-700" : "")}
              >
                <span
                  className={cx(
                    "flex size-5 items-center justify-center rounded-full text-[10px]",
                    i < step ? "bg-accent-600 text-white" : i === step ? "bg-zinc-900 text-white" : "bg-zinc-200 text-zinc-600",
                  )}
                >
                  {i < step ? <Check className="size-3" /> : i + 1}
                </span>
                {s}
              </button>
            ))}
          </div>
          <div className="h-1.5 overflow-hidden rounded-full bg-zinc-200">
            <div className="h-full rounded-full bg-accent-600 transition-[width] duration-300" style={{ width: `${((step + 1) / steps.length) * 100}%` }} />
          </div>
        </div>
        <div className="rounded-xl border border-zinc-200 bg-white p-6 shadow-sm sm:p-8">
          {step === 0 ? (
            <BrandStep
              onDone={next}
              onSkip={next}
              saving={update.isPending}
              save={(patch) => update.mutate({ ...patch, onboarding: { brand: true, sender: Boolean(patch.from_email) } }, { onSuccess: next })}
            />
          ) : step === 1 ? (
            <ContactsStep onDone={next} onSkip={next} markDone={() => update.mutate({ onboarding: { contacts: true } })} />
          ) : (
            <FlowStep onFinish={() => navigate("/")} markDone={() => update.mutate({ onboarding: { first_flow: true } })} />
          )}
        </div>
      </div>
    </div>
  );
}

function StepHeader({ title, description }: { title: string; description: string }) {
  return (
    <div className="mb-6">
      <h1 className="text-xl font-semibold tracking-tight text-zinc-950">{title}</h1>
      <p className="mt-1 text-sm text-zinc-500">{description}</p>
    </div>
  );
}

type BrandPatch = { website: string | null; from_name: string | null; from_email: string | null; brand_color: string; address: string | null };

function BrandStep({ save, saving, onSkip }: { save: (p: BrandPatch) => void; saving: boolean; onDone: () => void; onSkip: () => void }) {
  const { account, user } = useSession();
  const [color, setColor] = useState(account.brand_color || "#115f42");
  const onSubmit = (e: FormEvent<HTMLFormElement>) => {
    e.preventDefault();
    const d = new FormData(e.currentTarget);
    const str = (k: string) => String(d.get(k) ?? "").trim() || null;
    save({ website: str("website"), from_name: str("from_name"), from_email: str("from_email"), address: str("address"), brand_color: color });
  };
  return (
    <form onSubmit={onSubmit}>
      <StepHeader title="Your brand" description="Used in your emails, sign-up forms and footers. Change any of it later in Settings." />
      <div className="grid grid-cols-1 gap-4 sm:grid-cols-2">
        <Field label="Website">
          <Input name="website" type="text" inputMode="url" autoComplete="url" placeholder="acme.com" defaultValue={account.website ?? ""} autoFocus />
        </Field>
        <Field label="Sender name" hint="Who your emails come from.">
          <Input name="from_name" placeholder={account.name} defaultValue={account.from_name ?? account.name} />
        </Field>
        <Field label="Sender email" hint="Best on your own domain; you'll verify it in Settings.">
          <Input name="from_email" type="email" placeholder="hello@acme.com" defaultValue={account.from_email ?? user.email} />
        </Field>
        <Field label="Brand color">
          <ColorInput value={color} onChange={setColor} />
        </Field>
        <Field label="Mailing address" className="sm:col-span-2" hint="The law (CAN-SPAM) requires a physical postal address in every marketing email.">
          <Textarea name="address" rows={2} placeholder="123 Main St, Portland, OR 97201" defaultValue={account.address ?? ""} />
        </Field>
      </div>
      <StepFooter onSkip={onSkip}>
        <Button type="submit" variant="primary" loading={saving}>
          Continue
        </Button>
      </StepFooter>
    </form>
  );
}

function StepFooter({ onSkip, children }: { onSkip: () => void; children: ReactNode }) {
  return (
    <div className="mt-8 flex items-center justify-between border-t border-zinc-100 pt-5">
      <button type="button" onClick={onSkip} className="text-sm font-medium text-zinc-500 hover:text-zinc-900">
        Skip
      </button>
      <div className="flex gap-2">{children}</div>
    </div>
  );
}

function ContactsStep({ onDone, onSkip, markDone }: { onDone: () => void; onSkip: () => void; markDone: () => void }) {
  const importer = useImportProfiles();
  const [csv, setCsv] = useState("");
  const [fileName, setFileName] = useState<string | null>(null);
  const [dragging, setDragging] = useState(false);
  const [result, setResult] = useState<ImportResult | null>(null);

  const readFile = async (file: File | undefined) => {
    if (!file) return;
    setFileName(file.name);
    setCsv(await file.text());
    setResult(null);
  };

  const runImport = () =>
    importer.mutate(
      { csv, email_consent: "subscribed" },
      {
        onSuccess: (r) => {
          setResult(r);
          if (r.created + r.updated > 0) markDone();
        },
      },
    );

  return (
    <div>
      <StepHeader title="Bring your contacts" description="Paste a CSV or drop a file. We match columns like email, phone_number, first_name and last_name; anything else becomes a custom property." />
      {result ? (
        <div className="space-y-4">
          <div className="grid grid-cols-3 gap-3">
            {(
              [
                ["Created", result.created],
                ["Updated", result.updated],
                ["Skipped", result.skipped],
              ] as const
            ).map(([label, n]) => (
              <div key={label} className="rounded-lg border border-zinc-200 px-4 py-3">
                <div className="text-xs text-zinc-500">{label}</div>
                <div className="text-xl font-semibold tabular-nums">{formatNumber(n)}</div>
              </div>
            ))}
          </div>
          {result.errors.length > 0 ? (
            <div className="max-h-40 overflow-y-auto rounded-md bg-amber-50 p-3 text-xs text-amber-800">
              {result.errors.slice(0, 50).map((e, i) => (
                <div key={i}>{e}</div>
              ))}
            </div>
          ) : null}
          <StepFooter onSkip={onSkip}>
            <Button variant="ghost" onClick={() => setResult(null)}>
              Import more
            </Button>
            <Button variant="primary" onClick={onDone}>
              Continue
            </Button>
          </StepFooter>
        </div>
      ) : (
        <>
          <label
            onDragOver={(e) => {
              e.preventDefault();
              setDragging(true);
            }}
            onDragLeave={() => setDragging(false)}
            onDrop={(e) => {
              e.preventDefault();
              setDragging(false);
              void readFile(e.dataTransfer.files[0]);
            }}
            className={cx(
              "mb-3 flex cursor-pointer flex-col items-center justify-center gap-1 rounded-lg border-2 border-dashed px-4 py-6 text-center transition-colors",
              dragging ? "border-accent-500 bg-accent-50" : "border-zinc-300 hover:border-zinc-400",
            )}
          >
            <FileUp className="size-5 text-zinc-400" />
            <span className="text-sm font-medium text-zinc-700">{fileName ?? "Drop a CSV file or click to choose"}</span>
            <span className="text-xs text-zinc-500">First row must be column headers.</span>
            <input type="file" accept=".csv,text/csv" className="sr-only" onChange={(e) => void readFile(e.target.files?.[0])} />
          </label>
          <Textarea
            rows={6}
            value={csv}
            onChange={(e) => {
              setCsv(e.target.value);
              setFileName(null);
            }}
            placeholder={"email,first_name,last_name\njane@example.com,Jane,Doe"}
            className="font-mono text-xs"
            aria-label="CSV text"
          />
          <p className="mt-2 text-xs text-zinc-500">Only import people who agreed to hear from you. They'll be marked as subscribed to email.</p>
          <div className="mt-3">
            <InlineError message={importer.error?.message} />
          </div>
          <StepFooter onSkip={onSkip}>
            <Button variant="ghost" onClick={onSkip}>
              Skip, I'll add later
            </Button>
            <Button variant="primary" disabled={!csv.trim()} loading={importer.isPending} onClick={runImport}>
              Import contacts
            </Button>
          </StepFooter>
        </>
      )}
    </div>
  );
}

function FlowStep({ onFinish, markDone }: { onFinish: () => void; markDone: () => void }) {
  const blueprints = useBlueprints();
  const create = useCreateFlowFromBlueprint();
  const setStatus = useSetFlowStatus();
  const [live, setLive] = useState<Record<string, boolean>>({});
  // Flows already created from a blueprint; if going live fails, a retry reuses the flow instead of duplicating it.
  const [created, setCreated] = useState<Record<string, string>>({});
  const [busy, setBusy] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);

  const goLive = (key: string, flowId: string) =>
    setStatus.mutate(
      { id: flowId, status: "live" },
      {
        onSuccess: () => {
          setLive((l) => ({ ...l, [key]: true }));
          setError(null);
          markDone();
        },
        onError: (e) => setError(e.message),
        onSettled: () => setBusy(null),
      },
    );

  const turnOn = (key: string) => {
    setBusy(key);
    const existing = created[key];
    if (existing) return goLive(key, existing);
    create.mutate(
      { key },
      {
        onSuccess: (flow) => {
          setCreated((c) => ({ ...c, [key]: flow.id }));
          goLive(key, flow.id);
        },
        onError: (e) => {
          setError(e.message);
          setBusy(null);
        },
      },
    );
  };

  return (
    <div>
      <StepHeader title="Turn on a starter flow" description="Automations that earn money while you sleep. Each one is ready to go and you can edit it any time." />
      <div className="grid grid-cols-1 gap-3 sm:grid-cols-2">
        {blueprints.isPending
          ? [0, 1, 2, 3].map((i) => <Skeleton key={i} className="h-32" />)
          : (blueprints.data ?? []).map((b) => (
              <div key={b.key} className={cx("flex flex-col rounded-lg border p-4", live[b.key] ? "border-accent-300 bg-accent-50/40" : "border-zinc-200")}>
                <div className="flex items-start justify-between gap-2">
                  <div className="flex items-center gap-2">
                    <Sparkles className="size-4 text-accent-600" />
                    <h3 className="text-sm font-semibold text-zinc-900">{b.name}</h3>
                  </div>
                  <ChannelIcons channels={blueprintChannels(b)} />
                </div>
                <p className="mt-1.5 flex-1 text-xs leading-relaxed text-zinc-600">{b.description}</p>
                <div className="mt-3">
                  {live[b.key] ? (
                    <span className="inline-flex items-center gap-1 text-xs font-medium text-accent-700">
                      <Check className="size-3.5" /> Live
                    </span>
                  ) : (
                    <Button size="sm" variant="primary" loading={busy === b.key} disabled={busy !== null && busy !== b.key} onClick={() => turnOn(b.key)}>
                      Turn on
                    </Button>
                  )}
                </div>
              </div>
            ))}
      </div>
      <div className="mt-3 space-y-2">
        <InlineError message={blueprints.error?.message ?? error} />
      </div>
      <StepFooter onSkip={onFinish}>
        <Button variant="primary" onClick={onFinish}>
          Finish
        </Button>
      </StepFooter>
    </div>
  );
}
