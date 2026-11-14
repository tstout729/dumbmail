import { InlineTitle } from "../../components/ui/InlineTitle";
import { useState } from "react";
import { useBlocker, useNavigate, useParams } from "react-router";
import { ExternalLink, Monitor, Smartphone, Trash2, X } from "lucide-react";
import type { Form, FormConfig, FormField, FormKind } from "../../api";
import { useDeleteForm, useForm, useLists, useUpdateForm } from "../../api";
import {
  Button,
  Card,
  Checkbox,
  CodeBlock,
  ColorInput,
  ConfirmDialog,
  CopyField,
  Field,
  Input,
  Page,
  PageHeader,
  Segmented,
  Select,
  Skeleton,
  StatusBadge,
  Tabs,
  Textarea,
  cx,
} from "../../components/ui";
import { formatNumber, formatPercent } from "../../lib/format";
import { toast } from "../../lib/toast";
import { formKinds } from "./FormsPage";

export function FormEditorPage() {
  const { id = "" } = useParams();
  const form = useForm(id);
  if (form.isPending) {
    return (
      <Page wide>
        <Skeleton className="mb-6 h-7 w-64" />
        <div className="grid gap-4 lg:grid-cols-[400px_1fr]">
          <Skeleton className="h-[520px]" />
          <Skeleton className="h-[520px]" />
        </div>
      </Page>
    );
  }
  if (form.error) {
    return (
      <Page>
        <PageHeader title="Form" breadcrumbs={[{ label: "Sign-up forms", to: "/forms" }]} />
        <p className="text-sm text-red-600">{form.error.message}</p>
      </Page>
    );
  }
  return <FormEditor key={form.data.id} form={form.data} />;
}

type Draft = Pick<Form, "name" | "kind" | "list_id" | "config">;

const fieldLabels: Record<FormField, string> = { email: "Email", phone_number: "Phone number", first_name: "First name" };

function FormEditor({ form }: { form: Form }) {
  const lists = useLists();
  const update = useUpdateForm(form.id);
  const remove = useDeleteForm();
  const navigate = useNavigate();
  const [draft, setDraft] = useState<Draft | null>(null);
  const [tab, setTab] = useState<"design" | "install">("design");
  const [confirmDelete, setConfirmDelete] = useState(false);
  const current: Draft = draft ?? { name: form.name, kind: form.kind, list_id: form.list_id, config: form.config };
  const dirty = draft !== null;
  const c = current.config;
  const blocker = useBlocker(
    ({ currentLocation, nextLocation }) =>
      dirty && currentLocation.pathname !== nextLocation.pathname && !(nextLocation.state as { saved?: boolean } | null)?.saved,
  );

  const edit = (patch: Partial<Draft>) => setDraft({ ...current, ...patch });
  const setConfig = (patch: Partial<FormConfig>) => edit({ config: { ...c, ...patch } });
  const toggleField = (f: FormField, on: boolean) => {
    const collect = on ? [...c.collect, f] : c.collect.filter((x) => x !== f);
    setConfig({ collect: collect.length ? collect : ["email"] });
  };

  const save = (status?: Form["status"]) =>
    update.mutate(
      { ...(draft ?? {}), ...(status ? { status } : {}) },
      {
        onSuccess: () => {
          setDraft(null);
          toast.success(status === "live" ? "Form published" : status === "draft" ? "Form unpublished" : "Form saved");
        },
      },
    );

  const origin = window.location.origin;
  const snippet = `<script async src="${origin}/f/${form.id}/embed.js"></script>`;
  const hosted = `${origin}/f/${form.id}`;

  return (
    <Page wide>
      <PageHeader
        breadcrumbs={[{ label: "Sign-up forms", to: "/forms" }]}
        title={
          <span className="flex items-center gap-2">
            <InlineTitle
              value={current.name}
              onChange={(e) => edit({ name: e.target.value })}
              aria-label="Form name"
            />
            <StatusBadge status={form.status} />
          </span>
        }
        description={`${formatNumber(form.stats.views)} views · ${formatNumber(form.stats.submits)} submits${form.stats.views ? ` · ${formatPercent(form.stats.submits / form.stats.views)} rate` : ""}`}
        actions={
          <>
            <Button variant="ghost" icon={<Trash2 className="size-3.5" />} onClick={() => setConfirmDelete(true)}>
              Delete
            </Button>
            <Button variant="secondary" disabled={!dirty} loading={update.isPending && !update.variables?.status} onClick={() => save()}>
              {dirty ? "Save" : "Saved"}
            </Button>
            {form.status === "live" ? (
              <Button variant="secondary" onClick={() => save("draft")}>
                Unpublish
              </Button>
            ) : (
              <Button variant="primary" loading={update.isPending && update.variables?.status === "live"} onClick={() => save("live")}>
                Publish
              </Button>
            )}
          </>
        }
      />
      <Tabs
        value={tab}
        onChange={setTab}
        tabs={[
          { value: "design", label: "Design" },
          { value: "install", label: "Install" },
        ]}
      />
      {tab === "design" ? (
        <div className="grid gap-4 lg:grid-cols-[400px_1fr]">
          <div className="space-y-4">
            <Card title="Setup">
              <div className="space-y-3">
                <Field label="Type">
                  <Select value={current.kind} onChange={(e) => edit({ kind: e.target.value as FormKind })}>
                    {formKinds.map((k) => (
                      <option key={k.value} value={k.value}>
                        {k.label}: {k.detail}
                      </option>
                    ))}
                  </Select>
                </Field>
                <Field label="Add sign-ups to list">
                  <Select value={current.list_id ?? ""} onChange={(e) => edit({ list_id: e.target.value || null })}>
                    <option value="">No list (profiles only)</option>
                    {(lists.data ?? []).map((l) => (
                      <option key={l.id} value={l.id}>
                        {l.name}
                      </option>
                    ))}
                  </Select>
                </Field>
                {current.kind === "popup" || current.kind === "flyout" ? (
                  <Field label="Show after (seconds)">
                    <Input type="number" min={0} value={c.delay_seconds ?? 0} onChange={(e) => setConfig({ delay_seconds: Math.max(0, Number(e.target.value) || 0) })} className="w-28" />
                  </Field>
                ) : null}
              </div>
            </Card>
            <Card title="Content">
              <div className="space-y-3">
                <Field label="Headline">
                  <Input value={c.headline} onChange={(e) => setConfig({ headline: e.target.value })} />
                </Field>
                <Field label="Body">
                  <Textarea rows={3} value={c.body} onChange={(e) => setConfig({ body: e.target.value })} />
                </Field>
                <Field label="Button text">
                  <Input value={c.button_text} onChange={(e) => setConfig({ button_text: e.target.value })} />
                </Field>
                <Field label="Success message">
                  <Input value={c.success_message} onChange={(e) => setConfig({ success_message: e.target.value })} />
                </Field>
              </div>
            </Card>
            <Card title="Fields">
              <div className="space-y-2">
                {(Object.keys(fieldLabels) as FormField[]).map((f) => (
                  <Checkbox key={f} label={fieldLabels[f]} checked={c.collect.includes(f)} onChange={(e) => toggleField(f, e.target.checked)} />
                ))}
                {c.collect.includes("phone_number") ? (
                  <Field label="SMS consent text" hint="Shown under the phone field. Carriers require clear consent language." className="pt-2">
                    <Textarea
                      rows={3}
                      value={c.sms_consent_text ?? ""}
                      onChange={(e) => setConfig({ sms_consent_text: e.target.value || undefined })}
                      placeholder="By submitting, you agree to receive recurring automated marketing texts at the number provided. Consent is not a condition of purchase. Msg & data rates may apply. Reply STOP to opt out."
                    />
                  </Field>
                ) : null}
              </div>
            </Card>
            <Card title="Style">
              <div className="space-y-3">
                <Field label="Brand color">
                  <ColorInput value={c.brand_color ?? "#115f42"} onChange={(brand_color) => setConfig({ brand_color })} />
                </Field>
                <Field label="Image URL (optional)">
                  <Input value={c.image_url ?? ""} onChange={(e) => setConfig({ image_url: e.target.value || undefined })} placeholder="https://…/hero.jpg" />
                </Field>
              </div>
            </Card>
          </div>
          <FormPreview kind={current.kind} config={c} />
        </div>
      ) : (
        <div className="grid gap-4 lg:grid-cols-2">
          <Card title="Embed on your site">
            <p className="mb-3 text-sm text-zinc-600">
              Paste this before the closing <code className="rounded bg-zinc-100 px-1 text-xs">&lt;/body&gt;</code> tag on every page.
              {current.kind === "embed" ? " The form renders where the script tag sits." : " It shows the form as a " + current.kind + "."}
            </p>
            <CodeBlock code={snippet} label="HTML" />
            {form.status !== "live" ? <p className="mt-3 text-xs text-amber-700">The form is a draft. Publish it before it shows on your site.</p> : null}
          </Card>
          <Card title="Hosted sign-up page">
            <p className="mb-3 text-sm text-zinc-600">Share this link anywhere: bio links, QR codes, receipts.</p>
            <CopyField value={hosted} />
            <a href={hosted} target="_blank" rel="noreferrer" className="mt-3 inline-flex items-center gap-1 text-sm font-medium text-accent-700 hover:underline">
              Open page <ExternalLink className="size-3.5" />
            </a>
          </Card>
        </div>
      )}
      {confirmDelete ? (
        <ConfirmDialog
          title="Delete this form?"
          body="It stops showing on your site right away."
          confirmLabel="Delete form"
          danger
          loading={remove.isPending}
          onConfirm={() => remove.mutate(form.id, { onSuccess: () => navigate("/forms", { state: { saved: true } }) })}
          onClose={() => setConfirmDelete(false)}
        />
      ) : null}
      {blocker.state === "blocked" ? (
        <ConfirmDialog
          title="Leave without saving?"
          body="Your changes to this form haven't been saved."
          confirmLabel="Discard changes"
          danger
          onConfirm={() => blocker.proceed()}
          onClose={() => blocker.reset()}
        />
      ) : null}
    </Page>
  );
}

function FormPreview({ kind, config }: { kind: FormKind; config: FormConfig }) {
  const [device, setDevice] = useState<"desktop" | "mobile">("desktop");
  const [submitted, setSubmitted] = useState(false);
  const color = config.brand_color || "#115f42";

  const formBody = (
    <div className={cx("relative overflow-hidden rounded-xl bg-white text-left shadow-2xl ring-1 ring-black/5", kind === "page" || kind === "embed" ? "shadow-none ring-zinc-200" : "")}>
      {kind === "popup" || kind === "flyout" ? (
        <span className="absolute right-2 top-2 rounded-full p-1 text-zinc-400" aria-hidden>
          <X className="size-4" />
        </span>
      ) : null}
      {config.image_url ? <img src={config.image_url} alt="" className="h-32 w-full object-cover" /> : null}
      <div className="p-6">
        {submitted ? (
          <div className="py-6 text-center">
            <div className="text-lg font-semibold text-zinc-900">{config.success_message}</div>
            <button type="button" className="mt-3 text-xs text-zinc-500 underline" onClick={() => setSubmitted(false)}>
              Reset preview
            </button>
          </div>
        ) : (
          <>
            <h3 className="text-xl font-bold tracking-tight text-zinc-950">{config.headline}</h3>
            <p className="mt-1.5 text-sm text-zinc-600">{config.body}</p>
            <form
              className="mt-4 space-y-2"
              onSubmit={(e) => {
                e.preventDefault();
                setSubmitted(true);
              }}
            >
              {config.collect.includes("first_name") ? <input className="h-10 w-full rounded-md border border-zinc-300 px-3 text-sm" placeholder="First name" /> : null}
              {config.collect.includes("email") ? <input className="h-10 w-full rounded-md border border-zinc-300 px-3 text-sm" placeholder="Email address" type="email" /> : null}
              {config.collect.includes("phone_number") ? <input className="h-10 w-full rounded-md border border-zinc-300 px-3 text-sm" placeholder="Phone number" type="tel" /> : null}
              {config.collect.includes("phone_number") && config.sms_consent_text ? <p className="text-[10px] leading-snug text-zinc-500">{config.sms_consent_text}</p> : null}
              <button type="submit" className="h-10 w-full rounded-md text-sm font-semibold text-white" style={{ background: color }}>
                {config.button_text}
              </button>
            </form>
          </>
        )}
      </div>
    </div>
  );

  return (
    <div className="flex min-h-0 flex-col">
      <div className="mb-2 flex items-center justify-between">
        <span className="text-xs text-zinc-500">Preview · {formKinds.find((k) => k.value === kind)?.label}</span>
        <Segmented
          size="sm"
          value={device}
          onChange={setDevice}
          options={[
            { value: "desktop", label: <><Monitor className="size-3.5" /> Desktop</> },
            { value: "mobile", label: <><Smartphone className="size-3.5" /> Mobile</> },
          ]}
        />
      </div>
      <div className="flex justify-center rounded-lg border border-zinc-200 bg-zinc-100 p-4">
        <div className={cx("relative h-[600px] overflow-hidden rounded-lg bg-white shadow-sm transition-[width]", device === "mobile" ? "w-[375px] max-w-full" : "w-full")}>
          {kind === "page" ? (
            <div className="flex h-full items-center justify-center p-6" style={{ background: `${color}14` }}>
              <div className="w-full max-w-sm">{formBody}</div>
            </div>
          ) : (
            <>
              <FakeSite />
              {kind === "embed" ? (
                <div className="absolute inset-x-0 top-[260px] px-6">
                  <div className="mx-auto max-w-md">{formBody}</div>
                </div>
              ) : kind === "popup" ? (
                <div className="absolute inset-0 flex items-center justify-center bg-zinc-950/40 p-6">
                  <div className="w-full max-w-sm">{formBody}</div>
                </div>
              ) : (
                <div className="absolute bottom-4 right-4 w-[min(320px,calc(100%-2rem))]">{formBody}</div>
              )}
            </>
          )}
        </div>
      </div>
    </div>
  );
}

function FakeSite() {
  return (
    <div className="pointer-events-none select-none p-6" aria-hidden>
      <div className="mb-8 flex items-center justify-between">
        <div className="h-4 w-24 rounded bg-zinc-200" />
        <div className="flex gap-3">
          <div className="h-3 w-12 rounded bg-zinc-100" />
          <div className="h-3 w-12 rounded bg-zinc-100" />
          <div className="h-3 w-12 rounded bg-zinc-100" />
        </div>
      </div>
      <div className="mb-3 h-8 w-2/3 rounded bg-zinc-200" />
      <div className="mb-2 h-3 w-full rounded bg-zinc-100" />
      <div className="mb-8 h-3 w-5/6 rounded bg-zinc-100" />
      <div className="grid grid-cols-3 gap-4">
        {[0, 1, 2].map((i) => (
          <div key={i} className="h-36 rounded-lg bg-zinc-100" />
        ))}
      </div>
    </div>
  );
}
