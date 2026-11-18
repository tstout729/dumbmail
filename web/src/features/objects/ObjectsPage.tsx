import { useState } from "react";
import { useNavigate } from "react-router";
import { Boxes, Plus } from "lucide-react";
import { Button, EmptyRow, EmptyState, Field, InlineError, Input, Modal, Page, PageHeader, RowLink, SkeletonRows, Table, Td, Th } from "../../components/ui";
import { formatNumber, pluralize } from "../../lib/format";
import { useCan } from "../team/api";
import { useObjectTypes, useSaveObjectType } from "./api";

export function ObjectsPage() {
  const types = useObjectTypes();
  const canEdit = useCan("settings.write");
  const [creating, setCreating] = useState(false);
  const rows = types.data?.data ?? [];
  return (
    <Page>
      <PageHeader
        title="Objects"
        description="Records of your own, like reservations or subscriptions, with their own fields and dates. Each can belong to one or more people."
        actions={
          canEdit ? (
            <Button variant="primary" icon={<Plus className="size-3.5" />} onClick={() => setCreating(true)}>
              New type
            </Button>
          ) : null
        }
      />
      <Table>
        <thead>
          <tr>
            <Th>Type</Th>
            <Th>Objects</Th>
            <Th className="hidden sm:table-cell">Fields</Th>
          </tr>
        </thead>
        <tbody>
          {types.isPending ? (
            <SkeletonRows cols={3} rows={3} />
          ) : types.error ? (
            <EmptyRow cols={3}>
              <EmptyState title="Couldn't load object types" description={types.error.message} />
            </EmptyRow>
          ) : rows.length === 0 ? (
            <EmptyRow cols={3}>
              <EmptyState
                icon={<Boxes className="size-5" />}
                title="No objects yet"
                description="Send objects with the API, MCP or an integration (the Guesty recipe keeps reservations), and their type appears here. Or set up a type first."
                action={canEdit ? <Button onClick={() => setCreating(true)}>New type</Button> : undefined}
              />
            </EmptyRow>
          ) : (
            rows.map((t) => (
              <RowLink key={t.id} to={`/audience/objects/${t.key}`}>
                <Td>
                  <div className="font-medium text-zinc-900">{t.plural}</div>
                  <div className="font-mono text-xs text-zinc-500">{t.key}</div>
                </Td>
                <Td className="tabular-nums text-zinc-700">{formatNumber(t.object_count ?? 0)}</Td>
                <Td className="hidden text-zinc-600 sm:table-cell">{pluralize(t.fields.length, "field")}</Td>
              </RowLink>
            ))
          )}
        </tbody>
      </Table>
      {creating ? <NewTypeDialog onClose={() => setCreating(false)} /> : null}
    </Page>
  );
}

function NewTypeDialog({ onClose }: { onClose: () => void }) {
  const save = useSaveObjectType();
  const navigate = useNavigate();
  const [name, setName] = useState("");
  const [key, setKey] = useState("");
  const [touchedKey, setTouchedKey] = useState(false);
  const derived = name
    .trim()
    .toLowerCase()
    .replace(/[^a-z0-9]+/g, "_")
    .replace(/^_+|_+$/g, "")
    .replace(/^[^a-z]+/, "");
  const k = touchedKey ? key : derived;
  const problem = !name.trim() ? "Give the type a name." : !/^[a-z][a-z0-9_]{0,39}$/.test(k) ? "The key is lower-case letters, digits and _, starting with a letter." : null;
  const [tried, setTried] = useState(false);
  const submit = () => {
    setTried(true);
    if (problem) return;
    save.mutate({ key: k, name: name.trim() }, { onSuccess: (t) => navigate(`/audience/objects/${t.key}?tab=fields`) });
  };
  return (
    <Modal
      title="New object type"
      description="Add its fields on the next screen."
      onClose={onClose}
      footer={
        <>
          <Button variant="ghost" onClick={onClose}>
            Cancel
          </Button>
          <Button variant="primary" loading={save.isPending} onClick={submit}>
            Create type
          </Button>
        </>
      }
    >
      <div className="space-y-3">
        <Field label="Name" hint="One of them, e.g. Reservation">
          <Input value={name} onChange={(e) => setName(e.target.value)} placeholder="Reservation" autoFocus />
        </Field>
        <Field label="Key" hint="Used by the API, flows and segments. It can't change later.">
          <Input
            value={k}
            onChange={(e) => {
              setTouchedKey(true);
              setKey(e.target.value);
            }}
            className="font-mono"
            placeholder="reservation"
          />
        </Field>
        <InlineError message={tried ? (problem ?? save.error?.message) : save.error?.message} />
      </div>
    </Modal>
  );
}
