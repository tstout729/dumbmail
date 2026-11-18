import { useState, type FormEvent, type ReactNode } from "react";
import { Link, useNavigate } from "react-router";
import { Check, FileUp, Sparkles } from "lucide-react";
import type { BrandKit, ImportResult } from "../../api";
import { socialNetworks } from "../../api";
import {
  BrandEmailPreview,
  ColorSwatches,
  LogoThumb,
  SocialChips,
  isFreeMail,
  workDomain,
  type SocialLinks,
} from "../../components/brand/BrandParts";
import { hostnameOf } from "../settings/shared";
import { ChannelIcons, blueprintChannels } from "../../components/flows/channels";
import {
  useBlueprints,
  useCreateFlowFromBlueprint,
  useImportBrand,
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

type BrandPatch = {
  name?: string;
  website: string | null;
  from_name: string | null;
  from_email: string | null;
  brand_color: string;
  address: string | null;
  logo_url: string | null;
  social_links: SocialLinks;
};

type BrandDraft = { website: string; name: string; from_email: string; brand_color: string; address: string; logo_url: string; social_links: SocialLinks };

/** Step 1: paste a website and we fill in the brand, then the user checks it over. */
function BrandStep({ save, saving, onSkip }: { save: (p: BrandPatch) => void; saving: boolean; onDone: () => void; onSkip: () => void }) {
  const { account, user } = useSession();
  const importer = useImportBrand();
  const hasBrand = Boolean(account.logo_url || account.address);
  const [phase, setPhase] = useState<"url" | "review">(hasBrand ? "review" : "url");
  const [site, setSite] = useState(() => hostnameOf(account.website) || workDomain(user.email) || "");
  const [found, setFound] = useState<BrandKit | null>(null);
  const [draft, setDraft] = useState<BrandDraft>(() => ({
    website: account.website ?? "",
    name: account.from_name ?? account.name,
    from_email: account.from_email ?? (isFreeMail(user.email) ? "" : user.email),
    brand_color: account.brand_color || "#1f6f5c",
    address: account.address ?? "",
    logo_url: account.logo_url ?? "",
    social_links: account.social_links ?? {},
  }));
  const set = (patch: Partial<BrandDraft>) => setDraft((d) => ({ ...d, ...patch }));

  const runImport = (url: string) =>
    importer.mutate(url, {
      onSuccess: (kit) => {
        setFound(kit);
        setDraft((d) => ({
          website: kit.website,
          name: kit.name ?? d.name,
          // Never replace a sender on the user's own domain; do replace an empty or Gmail-style one.
          from_email: kit.from_email && (!d.from_email || isFreeMail(d.from_email)) ? kit.from_email : d.from_email,
          brand_color: kit.brand_color ?? d.brand_color,
          address: kit.address ?? d.address,
          logo_url: kit.logo_url ?? d.logo_url,
          social_links: { ...d.social_links, ...kit.social_links },
        }));
        setPhase("review");
      },
    });

  const onSubmit = (e: FormEvent<HTMLFormElement>) => {
    e.preventDefault();
    const blank = (v: string) => v.trim() || null;
    // Send removed networks as "" so the server drops them.
    const social: SocialLinks = {};
    for (const n of socialNetworks) {
      if (draft.social_links[n] || account.social_links?.[n]) social[n] = draft.social_links[n] ?? "";
    }
    save({
      ...(draft.name.trim() ? { name: draft.name.trim() } : {}),
      website: blank(draft.website),
      from_name: blank(draft.name),
      from_email: blank(draft.from_email),
      brand_color: draft.brand_color,
      address: blank(draft.address),
      logo_url: blank(draft.logo_url),
      social_links: social,
    });
  };

  if (phase === "url") {
    return (
      <form
        onSubmit={(e) => {
          e.preventDefault();
          runImport(site);
        }}
      >
        <StepHeader title="Your brand" description="Paste your website and we'll pull in your logo, colors, contact email, address and social links. You can change anything after." />
        <div className="flex flex-col gap-2 sm:flex-row">
          <Input
            aria-label="Website"
            className="sm:flex-1"
            value={site}
            onChange={(e) => setSite(e.target.value)}
            inputMode="url"
            autoComplete="url"
            placeholder="yourstore.com"
            required
            autoFocus
          />
          <Button type="submit" variant="primary" loading={importer.isPending} icon={<Sparkles className="size-4" />}>
            Get my brand
          </Button>
        </div>
        {importer.isPending ? <p className="mt-3 text-sm text-zinc-500">Reading your site. This takes a few seconds.</p> : null}
        <div className="mt-3">
          <InlineError message={importer.error ? `${importer.error.message}. You can fill in your brand by hand instead.` : null} />
        </div>
        <StepFooter onSkip={onSkip}>
          <Button type="button" variant="ghost" onClick={() => setPhase("review")}>
            Fill in by hand
          </Button>
        </StepFooter>
      </form>
    );
  }

  const foundHost = found ? hostnameOf(found.website) : "";
  return (
    <form onSubmit={onSubmit}>
      <StepHeader
        title="Your brand"
        description={found ? `Here's what we found on ${foundHost}. Check it over; you can change anything later in Settings.` : "Used in your emails, sign-up forms and footers. Change any of it later in Settings."}
      />
      <div className="grid grid-cols-1 gap-4 sm:grid-cols-2">
        <Field label="Website" className="sm:col-span-2">
          <div className="flex gap-2">
            <Input className="min-w-0 flex-1" value={draft.website} onChange={(e) => set({ website: e.target.value })} inputMode="url" autoComplete="url" placeholder="yourstore.com" />
            <Button
              type="button"
              variant="secondary"
              loading={importer.isPending}
              disabled={!draft.website.trim()}
              icon={<Sparkles className="size-4" />}
              onClick={() => {
                setSite(draft.website);
                runImport(draft.website);
              }}
            >
              {found ? "Re-import" : "Import"}
            </Button>
          </div>
          <div className="mt-2 empty:hidden">
            <InlineError message={importer.error?.message} />
          </div>
        </Field>
        <Field label="Brand name" hint="Also who your emails come from.">
          <Input value={draft.name} onChange={(e) => set({ name: e.target.value })} placeholder={account.name} />
        </Field>
        <Field
          label="Sender email"
          hint={found?.from_email_found && draft.from_email === found.from_email ? "Listed on your site. You'll verify your domain in Settings." : "Best on your own domain; you'll verify it in Settings."}
        >
          <Input type="email" value={draft.from_email} onChange={(e) => set({ from_email: e.target.value })} placeholder="hello@yourstore.com" />
        </Field>
        <Field label="Logo" className="sm:col-span-2" hint={found && !found.logo_url ? "We couldn't find a logo image on your site. Paste a link to a PNG or JPG." : "A PNG or JPG link shows in every inbox."}>
          <div className="flex items-center gap-3">
            <LogoThumb url={draft.logo_url} />
            <Input className="min-w-0 flex-1" value={draft.logo_url} onChange={(e) => set({ logo_url: e.target.value })} placeholder="https://yourstore.com/logo.png" />
          </div>
        </Field>
        <Field label="Brand color" className="sm:col-span-2" hint={found?.colors.length ? "Colors from your site. Your buttons and links use this one." : "Your buttons and links use this color."}>
          <div className="flex flex-wrap items-center gap-3">
            <ColorSwatches colors={found?.colors ?? []} value={draft.brand_color} onPick={(c) => set({ brand_color: c })} />
            <ColorInput value={draft.brand_color} onChange={(c) => set({ brand_color: c })} />
          </div>
        </Field>
        <Field
          label="Mailing address"
          className="sm:col-span-2"
          hint={
            found && !found.address
              ? "We couldn't find one on your site. The law (CAN-SPAM) requires a postal address in every marketing email."
              : "The law (CAN-SPAM) requires a physical postal address in every marketing email."
          }
        >
          <Textarea rows={2} value={draft.address} onChange={(e) => set({ address: e.target.value })} placeholder="123 Main St, Portland, OR 97201" />
        </Field>
        {socialNetworks.some((n) => draft.social_links[n]) ? (
          <Field label="Social links" className="sm:col-span-2" hint="Shown in your email footers.">
            <SocialChips
              links={draft.social_links}
              onRemove={(n) =>
                setDraft((d) => {
                  const next = { ...d.social_links };
                  delete next[n];
                  return { ...d, social_links: next };
                })
              }
            />
          </Field>
        ) : null}
        <div className="sm:col-span-2">
          <p className="mb-1.5 text-xs font-medium text-zinc-500">Preview</p>
          <BrandEmailPreview name={draft.name} logoUrl={draft.logo_url} color={draft.brand_color} address={draft.address} socials={draft.social_links} />
        </div>
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
      <p className="mb-4 rounded-md border border-zinc-200 bg-zinc-50 px-3 py-2 text-sm text-zinc-600">
        Coming from Klaviyo?{" "}
        <Link to="/settings/klaviyo" className="font-medium text-accent-800 hover:underline">
          Move everything across in one step
        </Link>{" "}
        instead: profiles with their consent, lists, segments, history, templates, campaigns and flows.
      </p>
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
