import { useState, type FormEvent } from "react";
import { useNavigate, useSearchParams } from "react-router";
import { FileText, Plus } from "lucide-react";
import type { FormConfig, FormKind } from "../../api";
import { useCreateForm, useForms, useLists, useSession } from "../../api";
import {
  Badge,
  Button,
  EmptyRow,
  EmptyState,
  Field,
  Input,
  Modal,
  Page,
  PageHeader,
  RowLink,
  Select,
  SkeletonRows,
  StatusBadge,
  Table,
  Td,
  Th,
  cx,
} from "../../components/ui";
import { formatNumber, formatPercent, formatRelative } from "../../lib/format";

export const formKinds: { value: FormKind; label: string; detail: string }[] = [
  { value: "popup", label: "Popup", detail: "Centered over the page" },
  { value: "flyout", label: "Flyout", detail: "Slides in from a corner" },
  { value: "embed", label: "Embed", detail: "Inline, wherever you place it" },
  { value: "page", label: "Full page", detail: "Hosted link, no site needed" },
];

export function defaultFormConfig(brandColor: string): FormConfig {
  return {
    headline: "Get 10% off your first order",
    body: "Join our list for early access, new drops and members-only deals.",
    button_text: "Sign me up",
    success_message: "Thanks! Check your inbox for your code.",
    collect: ["email"],
    delay_seconds: 5,
    brand_color: brandColor,
  };
}

export function FormsPage() {
  const forms = useForms();
  const lists = useLists();
  const [params, setParams] = useSearchParams();
  const creating = params.get("create") === "1";
  const listName = (id: string | null) => (id ? (lists.data ?? []).find((l) => l.id === id)?.name ?? "—" : "—");

  const setCreating = (on: boolean) => {
    const next = new URLSearchParams(params);
    if (on) next.set("create", "1");
    else next.delete("create");
    setParams(next, { replace: true });
  };

  return (
    <Page>
      <PageHeader
        title="Sign-up forms"
        description="Grow your list with popups, flyouts, embeds and hosted pages."
        actions={
          <Button variant="primary" icon={<Plus className="size-4" />} onClick={() => setCreating(true)}>
            Create form
          </Button>
        }
      />
      <Table>
        <thead>
          <tr>
            <Th>Form</Th>
            <Th>Status</Th>
            <Th>Type</Th>
            <Th>List</Th>
            <Th className="text-right">Views</Th>
            <Th className="text-right">Submits</Th>
            <Th className="text-right">Rate</Th>
          </tr>
        </thead>
        <tbody>
          {forms.isPending ? (
            <SkeletonRows cols={7} />
          ) : (forms.data ?? []).length === 0 ? (
            <EmptyRow cols={7}>
              <EmptyState
                icon={<FileText className="size-5" />}
                title="No forms yet"
                description="A simple popup with an offer is the fastest way to grow your list."
                action={
                  <Button variant="primary" onClick={() => setCreating(true)}>
                    Create form
                  </Button>
                }
              />
            </EmptyRow>
          ) : (
            (forms.data ?? []).map((f) => (
              <RowLink key={f.id} to={`/forms/${f.id}`}>
                <Td>
                  <div className="flex items-center gap-2 font-medium text-zinc-900">
                    {f.name}
                    {f.config.variants?.length ? <Badge tone="violet">A/B test</Badge> : null}
                  </div>
                  <div className="text-xs text-zinc-500">Updated {formatRelative(f.updated_at)}</div>
                </Td>
                <Td>
                  <StatusBadge status={f.status} />
                </Td>
                <Td>
                  <Badge>{formKinds.find((k) => k.value === f.kind)?.label ?? f.kind}</Badge>
                </Td>
                <Td className="text-zinc-600">{listName(f.list_id)}</Td>
                <Td className="text-right tabular-nums">{formatNumber(f.stats.views)}</Td>
                <Td className="text-right tabular-nums">{formatNumber(f.stats.submits)}</Td>
                <Td className="text-right tabular-nums">{f.stats.views ? formatPercent(f.stats.submits / f.stats.views) : "—"}</Td>
              </RowLink>
            ))
          )}
        </tbody>
      </Table>
      {creating ? <CreateFormDialog onClose={() => setCreating(false)} /> : null}
    </Page>
  );
}

function CreateFormDialog({ onClose }: { onClose: () => void }) {
  const { account } = useSession();
  const lists = useLists();
  const create = useCreateForm();
  const navigate = useNavigate();
  const [kind, setKind] = useState<FormKind>("popup");

  const onSubmit = (e: FormEvent<HTMLFormElement>) => {
    e.preventDefault();
    const d = new FormData(e.currentTarget);
    create.mutate(
      {
        name: String(d.get("name")).trim(),
        kind,
        list_id: String(d.get("list_id") ?? "") || null,
        config: defaultFormConfig(account.brand_color),
      },
      { onSuccess: (f) => navigate(`/forms/${f.id}`) },
    );
  };

  return (
    <Modal title="Create sign-up form" onClose={onClose} size="md">
      <form onSubmit={onSubmit} className="space-y-4">
        <div className="grid grid-cols-2 gap-2">
          {formKinds.map((k) => (
            <button
              key={k.value}
              type="button"
              onClick={() => setKind(k.value)}
              aria-pressed={kind === k.value}
              className={cx(
                "rounded-lg border p-3 text-left",
                kind === k.value ? "border-accent-500 bg-accent-50/50 ring-1 ring-accent-500" : "border-zinc-200 hover:border-zinc-300",
              )}
            >
              <div className="text-sm font-semibold text-zinc-900">{k.label}</div>
              <div className="text-xs text-zinc-500">{k.detail}</div>
            </button>
          ))}
        </div>
        <Field label="Form name">
          <Input name="name" required autoFocus defaultValue="Welcome popup" />
        </Field>
        <Field label="Add sign-ups to list">
          <Select name="list_id" defaultValue={lists.data?.[0]?.id ?? ""}>
            <option value="">No list (profiles only)</option>
            {(lists.data ?? []).map((l) => (
              <option key={l.id} value={l.id}>
                {l.name}
              </option>
            ))}
          </Select>
        </Field>
        <div className="flex justify-end gap-2">
          <Button variant="ghost" onClick={onClose}>
            Cancel
          </Button>
          <Button type="submit" variant="primary" loading={create.isPending}>
            Create and design
          </Button>
        </div>
      </form>
    </Modal>
  );
}
