import { useState } from "react";
import { Link, useNavigate, useParams } from "react-router";
import { Lock, Trash2 } from "lucide-react";
import { useSession } from "../../api";
import { Badge, Button, Card, ConfirmDialog, EmptyState, Notice, Page, PageHeader, Skeleton } from "../../components/ui";
import { formatDateTime, fullName } from "../../lib/format";
import { toast } from "../../lib/toast";
import { useCan } from "../team/api";
import { useDeleteObject, useObject, useObjectType } from "./api";
import { FieldList, fieldLabel, formatValue } from "./parts";
import type { FieldDef, ObjectChange } from "./types";

const sourceLabel = (s: string | null) => (!s ? "—" : s === "api" ? "the API" : s === "mcp" ? "MCP" : s.startsWith("integration:") ? "an integration" : s);

export function ObjectPage() {
  const { type = "", id = "" } = useParams();
  const { account } = useSession();
  const ty = useObjectType(type);
  const o = useObject(id);
  const canDelete = useCan("audience.write");
  const remove = useDeleteObject();
  const navigate = useNavigate();
  const [confirm, setConfirm] = useState(false);
  const back = [
    { label: "Objects", to: "/audience/objects" },
    { label: ty.data?.plural ?? type, to: `/audience/objects/${type}` },
  ];
  if (o.isPending || ty.isPending) {
    return (
      <Page>
        <Skeleton className="mb-2 h-7 w-64" />
        <Skeleton className="h-72" />
      </Page>
    );
  }
  if (o.error || ty.error) {
    return (
      <Page>
        <PageHeader title="Object" breadcrumbs={back} />
        <p className="text-sm text-red-600">{(o.error ?? ty.error)?.message}</p>
      </Page>
    );
  }
  const obj = o.data;
  const fields = ty.data.fields;
  const tz = account.timezone;
  return (
    <Page wide>
      <PageHeader
        breadcrumbs={back}
        title={obj.title || obj.external_id}
        description={
          <>
            <span className="font-mono">{obj.external_id}</span> · version {obj.version} · last written by {sourceLabel(obj.source)} {formatDateTime(obj.updated_at, tz)}
          </>
        }
        actions={
          canDelete ? (
            <Button variant="danger" icon={<Trash2 className="size-3.5" />} onClick={() => setConfirm(true)}>
              Delete
            </Button>
          ) : null
        }
      />
      <div className="grid grid-cols-1 gap-4 lg:grid-cols-[minmax(0,1fr)_340px]">
        <div className="space-y-4">
          <Card title="Fields">
            <FieldList fields={fields} values={obj.fields} timeZone={tz} />
            {obj.hidden_fields.length ? (
              <div className="mt-4">
                <Notice tone="gray" icon={<Lock className="size-4" />} title={`${obj.hidden_fields.map((k) => fieldLabel(fields.find((f) => f.key === k) ?? { key: k, type: "text" })).join(", ")} hidden`}>
                  Sensitive fields show only for people who can read profiles.
                </Notice>
              </div>
            ) : null}
          </Card>
          <Card title="History" padded={false}>
            {obj.history.length === 0 ? (
              <div className="p-4">
                <EmptyState title="No changes recorded" description="Changes older than a year, or beyond the newest 100, are trimmed." />
              </div>
            ) : (
              <ol className="divide-y divide-zinc-100">
                {obj.history.map((c) => (
                  <HistoryRow key={c.id} c={c} fields={fields} tz={tz} />
                ))}
              </ol>
            )}
          </Card>
        </div>
        <Card title="People" padded={false}>
          {obj.profiles === undefined ? (
            <p className="p-4 text-sm text-zinc-500">
              Linked to {obj.linked_count} {obj.linked_count === 1 ? "person" : "people"}. Seeing who needs permission to read profiles.
            </p>
          ) : obj.profiles.length === 0 ? (
            <p className="p-4 text-sm text-zinc-500">Not linked to anyone yet.</p>
          ) : (
            <ul className="divide-y divide-zinc-100">
              {obj.profiles.map((p) => (
                <li key={`${p.id}-${p.relationship}`}>
                  <Link to={`/audience/profiles/${p.id}`} className="flex items-center justify-between gap-3 px-4 py-3 hover:bg-zinc-50">
                    <span className="min-w-0">
                      <span className="block truncate text-sm font-medium text-zinc-900">{fullName(p) || p.email || p.phone_number || "Anonymous"}</span>
                      {fullName(p) && (p.email || p.phone_number) ? <span className="block truncate text-xs text-zinc-500">{p.email ?? p.phone_number}</span> : null}
                    </span>
                    {p.relationship ? <Badge>{p.relationship}</Badge> : null}
                  </Link>
                </li>
              ))}
            </ul>
          )}
        </Card>
      </div>
      {confirm ? (
        <ConfirmDialog
          title="Delete this object?"
          body="It's removed with its history. People in flows for it leave those flows. The people it belongs to stay."
          confirmLabel="Delete"
          danger
          loading={remove.isPending}
          onConfirm={() =>
            remove.mutate(obj.id, {
              onSuccess: () => {
                toast.success("Object deleted");
                navigate(`/audience/objects/${type}`);
              },
              onError: (e) => toast.error(e.message),
            })
          }
          onClose={() => setConfirm(false)}
        />
      ) : null}
    </Page>
  );
}

function HistoryRow({ c, fields, tz }: { c: ObjectChange; fields: FieldDef[]; tz: string }) {
  const def = (k: string) => fields.find((f) => f.key === k);
  return (
    <li className="px-4 py-3">
      <div className="flex flex-wrap items-baseline justify-between gap-2">
        <span className="text-sm font-medium text-zinc-900">{c.kind === "created" ? "Created" : `Changed ${c.changed.length === 1 ? fieldLabel(def(c.changed[0]) ?? { key: c.changed[0], type: "text" }) : `${c.changed.length} fields`}`}</span>
        <span className="text-xs text-zinc-500">
          {formatDateTime(c.at, tz)} · {sourceLabel(c.source)}
        </span>
      </div>
      {c.kind === "updated" ? (
        <ul className="mt-1 space-y-0.5">
          {c.changed.map((k) => (
            <li key={k} className="text-xs text-zinc-600">
              <span className="text-zinc-500">{fieldLabel(def(k) ?? { key: k, type: "text" })}:</span> <span className="line-through decoration-zinc-300">{formatValue(def(k), c.before[k], tz)}</span> → <span className="text-zinc-900">{formatValue(def(k), c.after[k], tz)}</span>
            </li>
          ))}
        </ul>
      ) : null}
    </li>
  );
}
