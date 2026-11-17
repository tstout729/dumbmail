import { useState } from "react";
import { useBlocker, useNavigate, useParams } from "react-router";
import { Settings2, Trash2 } from "lucide-react";
import type { Form, FormConfig, FormKind, FormVariant, VersionId } from "../../api";
import { useDeleteForm, useForm, useLists, useSession, useUpdateForm } from "../../api";
import { Button, CollapsibleCard, ConfirmDialog, Field, Page, PageHeader, Select, Skeleton, StatusBadge, Tabs } from "../../components/ui";
import { InlineTitle } from "../../components/ui/InlineTitle";
import { formatNumber, formatPercent } from "../../lib/format";
import { toast } from "../../lib/toast";
import { formKinds } from "./FormsPage";
import {
  AbResultsCard,
  AbTestCard,
  adoptVersion,
  newVariant,
  nextVariantId,
  removeVariant,
  sameVersionContent,
  versionConfig,
  versionIds,
  versionLabel,
  type VariantId,
} from "./FormAbTest";
import { ContentSection, SuccessSection } from "./ContentSections";
import { FieldsSection, PhoneStepSection } from "./FieldsSection";
import { StyleSection } from "./StyleSection";
import { TargetingSection, TeaserSection } from "./TargetingSection";
import { FormPreview } from "./FormPreview";
import { InstallPanel } from "./InstallPanel";
import { configForSave, fieldProblems, fieldsOf, flyoutPositions, popupPositions } from "./formConfig";
import { isOverlayKind, type Editor } from "./editorShared";

export function FormEditorPage() {
  const { id = "" } = useParams();
  const form = useForm(id);
  if (form.isPending) {
    return (
      <Page wide>
        <Skeleton className="mb-6 h-7 w-64" />
        <div className="grid grid-cols-1 gap-4 lg:grid-cols-[420px_minmax(0,1fr)]">
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
type SectionId = "setup" | "content" | "fields" | "phone" | "success" | "style" | "teaser" | "targeting";

/** A position the new kind can't use (popup "top" on a flyout) goes back to the default. */
function fitStyleToKind(config: FormConfig, kind: FormKind): FormConfig {
  const pos = config.style?.position;
  if (!pos) return config;
  const valid = (kind === "popup" ? popupPositions : kind === "flyout" ? flyoutPositions : []).some((p) => p.value === pos);
  return valid ? config : { ...config, style: { ...config.style, position: undefined } };
}

function FormEditor({ form }: { form: Form }) {
  const { account } = useSession();
  const lists = useLists();
  const update = useUpdateForm(form.id);
  const remove = useDeleteForm();
  const navigate = useNavigate();
  const [draft, setDraft] = useState<Draft | null>(null);
  const [tab, setTab] = useState<"design" | "install">("design");
  const [confirmDelete, setConfirmDelete] = useState(false);
  const [version, setVersion] = useState<VersionId>("a");
  const [open, setOpen] = useState<Set<SectionId>>(() => new Set(["setup", "content"]));
  const [confirmAb, setConfirmAb] = useState<{ kind: "end" } | { kind: "remove"; id: VariantId } | { kind: "use"; id: VersionId } | null>(null);
  const current: Draft = draft ?? { name: form.name, kind: form.kind, list_id: form.list_id, config: form.config };
  const dirty = draft !== null;
  const c = current.config;
  const blocker = useBlocker(
    ({ currentLocation, nextLocation }) =>
      dirty && currentLocation.pathname !== nextLocation.pathname && !(nextLocation.state as { saved?: boolean } | null)?.saved,
  );

  const edit = (patch: Partial<Draft>) => setDraft({ ...current, ...patch });
  const setConfig = (patch: Partial<FormConfig>) => edit({ config: { ...c, ...patch } });
  const toggle = (id: SectionId) =>
    setOpen((prev) => {
      const next = new Set(prev);
      if (next.has(id)) next.delete(id);
      else next.add(id);
      return next;
    });
  const section = (id: SectionId) => ({ open: open.has(id), onToggle: () => toggle(id) });

  const variants = c.variants ?? [];
  const versions = versionIds(c);
  // A saved result can be picked only while that letter still shows the same content as when it was counted
  // (removing or editing a version re-letters or changes it until saved).
  const savedVersions = versionIds(form.config);
  const pickable = versions.filter((v) => savedVersions.includes(v) && sameVersionContent(c, form.config, v));
  const testing = versions.length > 0;
  // The selected tab falls back to A when its version is removed or the test ends.
  const selected: VersionId = versions.includes(version) ? version : "a";
  const variant = variants.find((v) => v.id === selected);
  const setVariant = (patch: Partial<FormVariant>) => setConfig({ variants: variants.map((v) => (v.id === selected ? { ...v, ...patch } : v)) });
  const viewsFor = (id: VersionId) => form.variant_stats.find((s) => s.id === id)?.views ?? 0;

  const startTest = () => {
    setConfig({ variants: [newVariant(c, "b")] });
    setVersion("b");
    setOpen((prev) => new Set([...prev, "content"]));
  };
  const addVersion = () => {
    const id = nextVariantId(c);
    if (!id) return;
    setConfig({ variants: [...variants, newVariant(c, id)] });
    setVersion(id);
  };
  const endTest = () => {
    setConfig({ variants: [] });
    setVersion("a");
    setConfirmAb(null);
  };
  const doRemove = (id: VariantId) => {
    setConfig({ variants: removeVariant(variants, id) });
    setVersion("a");
    setConfirmAb(null);
  };
  // Removing a version re-letters the ones after it, and the server resets the counts of every version whose content changed: warn first.
  const requestRemove = (id: VariantId) => {
    const affected = variants.slice(variants.findIndex((v) => v.id === id));
    if (affected.some((v) => viewsFor(v.id) > 0)) setConfirmAb({ kind: "remove", id });
    else doRemove(id);
  };
  const requestEnd = () => (form.variant_stats.some((s) => s.views > 0) ? setConfirmAb({ kind: "end" }) : endTest());

  const problems = fieldProblems(fieldsOf(c));
  /** Name, kind, list and config when edited, plus a status change. The config goes whole; cleared keys go as null. */
  const payload = (d: Draft | null, status?: Form["status"]) => ({
    ...(d ? { name: d.name, kind: d.kind, list_id: d.list_id, config: configForSave(d.config, form.config) } : {}),
    ...(status ? { status } : {}),
  });
  const blocked = () => {
    if (!problems.length) return false;
    toast.error(problems[0]);
    setOpen((prev) => new Set([...prev, "fields"]));
    return true;
  };

  /** Copies the version's content into the form and ends the test, along with any other unsaved edits, in one save. */
  const applyVersion = (id: VersionId) => {
    if (blocked()) return;
    update.mutate(payload({ ...current, config: adoptVersion(c, id) }), {
      onSuccess: () => {
        setDraft(null);
        setConfirmAb(null);
        setVersion("a");
        toast.success(`${versionLabel(id)} is now the form`);
      },
    });
  };

  const save = (status?: Form["status"]) => {
    if (draft && blocked()) return;
    update.mutate(payload(draft, status), {
      onSuccess: () => {
        setDraft(null);
        toast.success(status === "live" ? "Form published" : status === "draft" ? "Form unpublished" : "Form saved");
      },
    });
  };

  const ed: Editor = {
    formId: form.id,
    config: c,
    kind: current.kind,
    set: setConfig,
    accountBrandColor: account.brand_color,
    timezone: account.timezone,
    testing,
    versions,
    version: selected,
    setVersion,
    variant,
    setVariant,
    removeVersion: requestRemove,
  };
  const overlay = isOverlayKind(current.kind);
  const kindInfo = formKinds.find((k) => k.value === current.kind);
  const listName = current.list_id ? (lists.data ?? []).find((l) => l.id === current.list_id)?.name : null;

  return (
    <Page wide>
      <PageHeader
        breadcrumbs={[{ label: "Sign-up forms", to: "/forms" }]}
        title={
          <span className="flex items-center gap-2">
            <InlineTitle value={current.name} onChange={(e) => edit({ name: e.target.value })} aria-label="Form name" />
            <StatusBadge status={form.status} />
          </span>
        }
        description={`${formatNumber(form.stats.views)} views · ${formatNumber(form.stats.submits)} sign-ups${form.stats.views ? ` · ${formatPercent(form.stats.submits / form.stats.views)} rate` : ""}`}
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
        <div className="grid grid-cols-1 gap-4 lg:grid-cols-[minmax(340px,420px)_minmax(0,1fr)]">
          <div className="min-w-0 space-y-3">
            <CollapsibleCard
              title="Setup"
              icon={<Settings2 />}
              summary={`${kindInfo?.label ?? current.kind} · ${listName ?? "No list"}`}
              {...section("setup")}
            >
              <Field label="Type">
                <Select
                  value={current.kind}
                  onChange={(e) => {
                    const kind = e.target.value as FormKind;
                    edit({ kind, config: fitStyleToKind(c, kind) });
                  }}
                >
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
            </CollapsibleCard>
            <AbTestCard config={c} savedConfig={form.config} onStart={startTest} onAdd={addVersion} onEnd={requestEnd} />
            <ContentSection ed={ed} {...section("content")} />
            <FieldsSection ed={ed} {...section("fields")} />
            <PhoneStepSection ed={ed} {...section("phone")} />
            <SuccessSection ed={ed} {...section("success")} />
            <StyleSection ed={ed} {...section("style")} />
            {overlay ? <TeaserSection ed={ed} {...section("teaser")} /> : null}
            {overlay ? <TargetingSection ed={ed} {...section("targeting")} /> : null}
          </div>
          <div className="min-w-0 space-y-4 lg:sticky lg:top-4 lg:self-start">
            <FormPreview formId={form.id} kind={current.kind} config={versionConfig(c, selected)} version={selected} testing={testing} accountBrandColor={account.brand_color} />
            {testing ? <AbResultsCard form={form} versions={pickable} onUse={(id) => setConfirmAb({ kind: "use", id })} /> : null}
          </div>
        </div>
      ) : (
        <InstallPanel form={form} kind={current.kind} />
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
      {confirmAb?.kind === "end" ? (
        <ConfirmDialog
          title="End the A/B test?"
          body="Every visitor sees version A again. Versions B and up and their results are deleted when you save."
          confirmLabel="End test"
          danger
          onConfirm={endTest}
          onClose={() => setConfirmAb(null)}
        />
      ) : confirmAb?.kind === "remove" ? (
        <ConfirmDialog
          title={`Remove version ${confirmAb.id.toUpperCase()}?`}
          body="Its results are deleted when you save. The versions after it move up a letter and their results start over."
          confirmLabel="Remove version"
          danger
          onConfirm={() => doRemove(confirmAb.id)}
          onClose={() => setConfirmAb(null)}
        />
      ) : confirmAb?.kind === "use" ? (
        <ConfirmDialog
          title={`Use version ${confirmAb.id.toUpperCase()} for everyone?`}
          body={
            confirmAb.id === "a"
              ? `The form keeps version A's content and the test ends. Its results are deleted.${dirty ? " Your other unsaved changes are saved too." : ""}`
              : `${versionLabel(confirmAb.id)}'s content (headline, text, button, image, "No thanks" link, thank-you message and discount code) becomes the form's and the test ends. Its results are deleted.${dirty ? " Your other unsaved changes are saved too." : ""}`
          }
          confirmLabel="Use this version"
          loading={update.isPending}
          onConfirm={() => applyVersion(confirmAb.id)}
          onClose={() => setConfirmAb(null)}
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
