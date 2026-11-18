import { useState } from "react";
import { useNavigate, useParams, useSearchParams } from "react-router";
import { Plus, Search, Trash2, X } from "lucide-react";
import { useSession } from "../../api";
import {
  Button,
  Checkbox,
  ConfirmDialog,
  EmptyRow,
  EmptyState,
  Field,
  IconButton,
  InlineError,
  Input,
  Notice,
  Page,
  PageHeader,
  RowLink,
  Select,
  Skeleton,
  SkeletonRows,
  Table,
  TagInput,
  Tabs,
  Td,
  Th,
} from "../../components/ui";
import { formatNumber, formatRelative, pluralize } from "../../lib/format";
import { toast } from "../../lib/toast";
import { ApiError } from "../../api/client";
import { useCan } from "../team/api";
import { useDeleteObjectType, useObjects, useObjectType, useSaveObjectType } from "./api";
import { fieldLabel, formatValue, keyFields, kindLabels } from "./parts";
import type { FieldDef, FieldKind, ObjectType } from "./types";

type Tab = "objects" | "fields";

export function ObjectTypePage() {
  const { type = "" } = useParams();
  const t = useObjectType(type);
  const [params, setParams] = useSearchParams();
  const tab: Tab = params.get("tab") === "fields" ? "fields" : "objects";
  if (t.isPending) {
    return (
      <Page>
        <Skeleton className="mb-2 h-7 w-56" />
        <Skeleton className="h-64" />
      </Page>
    );
  }
  if (t.error) {
    return (
      <Page>
        <PageHeader title="Object type" breadcrumbs={[{ label: "Objects", to: "/audience/objects" }]} />
        <p className="text-sm text-red-600">{t.error.message}</p>
      </Page>
    );
  }
  const ty = t.data;
  return (
    <Page wide>
      <PageHeader
        breadcrumbs={[{ label: "Objects", to: "/audience/objects" }]}
        title={ty.plural}
        description={
          <>
            <span className="font-mono">{ty.key}</span> · {pluralize(ty.object_count ?? 0, "object")} · {pluralize(ty.fields.length, "field")}
          </>
        }
      />
      <Tabs<Tab>
        value={tab}
        onChange={(v) => setParams(v === "fields" ? { tab: "fields" } : {}, { replace: true })}
        tabs={[
          { value: "objects", label: ty.plural, count: ty.object_count },
          { value: "fields", label: "Fields", count: ty.fields.length },
        ]}
      />
      {tab === "objects" ? <ObjectList ty={ty} /> : <FieldsEditor key={ty.updated_at} ty={ty} />}
    </Page>
  );
}

function ObjectList({ ty }: { ty: ObjectType }) {
  const { account } = useSession();
  const [q, setQ] = useState("");
  const [offset, setOffset] = useState(0);
  const list = useObjects(ty.key, q.trim(), offset);
  const cols = keyFields(ty.fields, ty.title_field);
  const rows = list.data?.data ?? [];
  const total = list.data?.total ?? 0;
  const colCount = 3 + cols.length;
  return (
    <>
      <div className="mb-3 flex max-w-sm items-center gap-2">
        <div className="relative w-full">
          <Search className="pointer-events-none absolute left-2.5 top-1/2 size-3.5 -translate-y-1/2 text-zinc-400" />
          <Input
            value={q}
            onChange={(e) => {
              setQ(e.target.value);
              setOffset(0);
            }}
            placeholder={`Search by title or ID`}
            className="pl-8"
            aria-label="Search"
          />
        </div>
      </div>
      <Table>
        <thead>
          <tr>
            <Th>{ty.name}</Th>
            {cols.map((c, i) => (
              <Th key={c.key} className={i > 0 ? "hidden md:table-cell" : "hidden sm:table-cell"}>
                {fieldLabel(c)}
              </Th>
            ))}
            <Th className="hidden sm:table-cell">Changes</Th>
            <Th className="text-right">Updated</Th>
          </tr>
        </thead>
        <tbody>
          {list.isPending ? (
            <SkeletonRows cols={colCount} rows={5} />
          ) : list.error ? (
            <EmptyRow cols={colCount}>
              <EmptyState title="Couldn't load objects" description={list.error.message} />
            </EmptyRow>
          ) : rows.length === 0 ? (
            <EmptyRow cols={colCount}>
              <EmptyState title={q ? "Nothing matches" : `No ${ty.plural.toLowerCase()} yet`} description={q ? "Try part of the title or the ID from your system." : "Objects arrive through the API, MCP or an integration."} />
            </EmptyRow>
          ) : (
            rows.map((o) => (
              <RowLink key={o.id} to={`/audience/objects/${ty.key}/${o.id}`}>
                <Td>
                  <div className="min-w-0 max-w-[12rem] sm:max-w-xs">
                    <div className="truncate font-medium text-zinc-900">{o.title || o.external_id}</div>
                    {o.title ? <div className="truncate font-mono text-xs text-zinc-500">{o.external_id}</div> : null}
                  </div>
                </Td>
                {cols.map((c, i) => (
                  <Td key={c.key} className={`${i > 0 ? "hidden md:table-cell" : "hidden sm:table-cell"} whitespace-nowrap text-zinc-700`}>
                    {formatValue(c, o.fields[c.key], account.timezone)}
                  </Td>
                ))}
                <Td className="hidden tabular-nums text-zinc-600 sm:table-cell">{formatNumber(o.version)}</Td>
                <Td className="whitespace-nowrap text-right text-zinc-500">{formatRelative(o.updated_at)}</Td>
              </RowLink>
            ))
          )}
        </tbody>
      </Table>
      {total > 50 ? (
        <div className="mt-3 flex items-center justify-between text-sm text-zinc-600">
          <span>
            {formatNumber(offset + 1)}–{formatNumber(Math.min(offset + 50, total))} of {formatNumber(total)}
          </span>
          <div className="flex gap-2">
            <Button size="sm" disabled={offset === 0} onClick={() => setOffset(Math.max(0, offset - 50))}>
              Previous
            </Button>
            <Button size="sm" disabled={offset + 50 >= total} onClick={() => setOffset(offset + 50)}>
              Next
            </Button>
          </div>
        </div>
      ) : null}
    </>
  );
}

const kinds = Object.keys(kindLabels) as FieldKind[];
const validKey = (k: string) => /^[A-Za-z][A-Za-z0-9_]{0,63}$/.test(k);

function FieldsEditor({ ty }: { ty: ObjectType }) {
  const canEdit = useCan("settings.write");
  const save = useSaveObjectType();
  const remove = useDeleteObjectType();
  const navigate = useNavigate();
  const [fields, setFields] = useState<FieldDef[]>(ty.fields);
  const [title, setTitle] = useState(ty.title_field ?? "");
  const [name, setName] = useState(ty.name);
  const [plural, setPlural] = useState(ty.plural);
  const [confirmDelete, setConfirmDelete] = useState(false);
  // A field's new type doesn't fit stored values: the server says how many; converting needs a yes.
  const [convertAsk, setConvertAsk] = useState<string | null>(null);
  const existing = new Set(ty.fields.map((f) => f.key));
  const keys = fields.map((f) => f.key.trim());
  const problem = !name.trim() || !plural.trim()
    ? "Give the type a name and a plural."
    : keys.some((k) => !validKey(k))
      ? "Each field key is letters, digits and _, starting with a letter."
      : new Set(keys).size !== keys.length
        ? "Two fields have the same key."
        : fields.some((f) => f.type === "money" && f.currency !== undefined && !/^[A-Z]{3}$/.test(f.currency))
          ? "A currency is a 3-letter code such as USD."
          : null;
  const set = (i: number, f: FieldDef) => setFields(fields.map((x, j) => (j === i ? f : x)));
  const titleChoices = fields.filter((f) => !f.sensitive && ["text", "enum", "number", "date", "datetime"].includes(f.type) && f.key.trim());
  const onSave = (convert = false) =>
    save.mutate(
      { key: ty.key, name: name.trim(), plural: plural.trim(), fields: fields.map(clean), title_field: titleChoices.some((f) => f.key === title) ? title : "", convert },
      {
        onSuccess: () => {
          setConvertAsk(null);
          toast.success("Fields saved");
        },
        onError: (e) => {
          if (e instanceof ApiError && e.status === 409) setConvertAsk(e.message);
        },
      },
    );
  return (
    <fieldset disabled={!canEdit} className="min-w-0 space-y-4">
      {!canEdit ? (
        <Notice tone="gray" title="Only people who can change settings can edit fields." />
      ) : null}
      <div className="grid max-w-xl grid-cols-1 gap-3 sm:grid-cols-2">
        <Field label="Name">
          <Input value={name} onChange={(e) => setName(e.target.value)} />
        </Field>
        <Field label="Plural">
          <Input value={plural} onChange={(e) => setPlural(e.target.value)} />
        </Field>
      </div>
      <div className="space-y-2">
        {fields.map((f, i) => (
          <div key={i} className="flex flex-wrap items-start gap-2 rounded-lg border border-zinc-200 bg-white p-3">
            <Field label="Key" className="w-full sm:w-40">
              <Input value={f.key} onChange={(e) => set(i, { ...f, key: e.target.value })} disabled={existing.has(f.key) && ty.fields[i]?.key === f.key} className="font-mono text-xs" />
            </Field>
            <Field label="Label" className="w-full sm:w-44">
              <Input value={f.label ?? ""} onChange={(e) => set(i, { ...f, label: e.target.value })} placeholder={f.key} />
            </Field>
            <Field label="Type" className="w-full sm:w-40">
              <Select value={f.type} onChange={(e) => set(i, { ...f, type: e.target.value as FieldKind })}>
                {kinds.map((k) => (
                  <option key={k} value={k}>
                    {kindLabels[k]}
                  </option>
                ))}
              </Select>
            </Field>
            {f.type === "enum" ? (
              <Field label="Options" hint={f.options_hidden ? "Hidden for a sensitive field; leave empty to keep them" : "Leave empty for any short text"} className="w-full sm:w-64">
                <TagInput value={f.options ?? []} onChange={(options) => set(i, { ...f, options })} placeholder="Add an option" aria-label="Options" />
              </Field>
            ) : null}
            {f.type === "money" ? (
              <Field label="Currency" className="w-28">
                <Input value={f.currency ?? ""} onChange={(e) => set(i, { ...f, currency: e.target.value.toUpperCase() || undefined })} placeholder="USD" maxLength={3} />
              </Field>
            ) : null}
            <div className="flex items-center gap-2 self-center pt-4">
              <Checkbox label="Sensitive" description="Never in emails or texts, never in lists, and hidden from people who can't read profiles" checked={Boolean(f.sensitive)} onChange={(e) => set(i, { ...f, sensitive: e.target.checked })} />
              <IconButton label="Remove field" size="sm" onClick={() => setFields(fields.filter((_, j) => j !== i))}>
                <X className="size-3.5" />
              </IconButton>
            </div>
          </div>
        ))}
        <Button size="sm" variant="ghost" icon={<Plus className="size-3" />} onClick={() => setFields([...fields, { key: "", type: "text" }])}>
          Add field
        </Button>
      </div>
      <Field label="Title field" hint="Shown as each object's name in lists" className="max-w-xs">
        <Select value={title} onChange={(e) => setTitle(e.target.value)}>
          <option value="">None (show the ID)</option>
          {titleChoices.map((f) => (
            <option key={f.key} value={f.key}>
              {fieldLabel(f)}
            </option>
          ))}
        </Select>
      </Field>
      <p className="text-xs text-zinc-500">Removing a field here keeps the values already stored on objects; new values for it add it back.</p>
      <InlineError message={problem ?? (convertAsk ? null : save.error?.message)} />
      {canEdit ? (
        <div className="flex flex-wrap justify-between gap-2 border-t border-zinc-200 pt-4">
          <Button variant="danger" icon={<Trash2 className="size-3.5" />} onClick={() => setConfirmDelete(true)}>
            Delete type
          </Button>
          <Button variant="primary" disabled={Boolean(problem)} loading={save.isPending} onClick={() => onSave()}>
            Save fields
          </Button>
        </div>
      ) : null}
      {convertAsk ? (
        <ConfirmDialog
          title="Convert stored values?"
          body={convertAsk}
          confirmLabel="Convert and save"
          danger
          loading={save.isPending}
          onConfirm={() => onSave(true)}
          onClose={() => setConvertAsk(null)}
        />
      ) : null}
      {confirmDelete ? (
        <ConfirmDialog
          title={`Delete ${ty.plural.toLowerCase()}?`}
          body={
            ty.used_by?.length
              ? `It's used by ${ty.used_by.join(", ")}. Change those first.`
              : `This permanently deletes the ${ty.name} type and its ${pluralize(ty.object_count ?? 0, "object")}, with their history. The people they belong to stay.`
          }
          confirmLabel="Delete"
          danger
          loading={remove.isPending}
          onConfirm={() =>
            remove.mutate(ty.key, {
              onSuccess: () => {
                toast.success(`${ty.plural} deleted`);
                navigate("/audience/objects");
              },
              onError: (e) => {
                toast.error(e.message);
                setConfirmDelete(false);
              },
            })
          }
          onClose={() => setConfirmDelete(false)}
        />
      ) : null}
    </fieldset>
  );
}

function clean(f: FieldDef): FieldDef {
  const out: FieldDef = { key: f.key.trim(), type: f.type };
  if (f.label?.trim()) out.label = f.label.trim();
  if (f.type === "enum" && f.options?.length) out.options = f.options;
  // `options_hidden` is only for showing: the server keeps hidden options when none are sent.
  if (f.type === "money" && f.currency) out.currency = f.currency;
  if (f.sensitive) out.sensitive = true;
  return out;
}
