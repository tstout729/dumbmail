import { useState, type FormEvent } from "react";
import { AlertTriangle, KeyRound, Plus, Trash2 } from "lucide-react";
import { errorMessage, useSession } from "../../api";
import {
  Badge,
  Button,
  Card,
  ConfirmDialog,
  CopyField,
  EmptyRow,
  EmptyState,
  Field,
  IconButton,
  InlineError,
  Input,
  Modal,
  Notice,
  Segmented,
  Select,
  SkeletonRows,
  Table,
  Td,
  Th,
} from "../../components/ui";
import { formatDate, formatRelative } from "../../lib/format";
import { useCan, useCreateScopedKey, useRevokeScopedKey, useScopedKeys } from "./api";
import type { CreatedScopedKey, ScopedApiKey } from "./types";

const areas = [
  { key: "audience", label: "Audience", hint: "Profiles, lists, segments, events" },
  { key: "messaging", label: "Messaging", hint: "Campaigns and flows, including sending" },
  { key: "content", label: "Content", hint: "Templates, forms, images" },
  { key: "analytics", label: "Analytics", hint: "Reports and metrics" },
  { key: "account", label: "Account", hint: "Settings, domains, billing, team, keys, activity log" },
] as const;

type Access = "none" | "read" | "write";

function scopeLabel(scope: string): string {
  if (scope === "full") return "Full access";
  if (scope === "read") return "Read everything";
  const [area, read] = scope.split(":");
  const name = areas.find((a) => a.key === area)?.label ?? area;
  return read ? `${name}: read` : name;
}

/** API keys with scopes, an expiry and who made them (Settings → Developers). */
export function ApiKeysCard() {
  const { account } = useSession();
  const canManage = useCan("api_keys.manage");
  const keys = useScopedKeys(canManage);
  const revoke = useRevokeScopedKey();
  const [creating, setCreating] = useState(false);
  const [created, setCreated] = useState<CreatedScopedKey | null>(null);
  const [revoking, setRevoking] = useState<ScopedApiKey | null>(null);
  const rows = keys.data?.data ?? [];

  if (!canManage) {
    return (
      <Card title="API keys">
        <Notice tone="gray" icon={<KeyRound />} title="Only owners and admins manage API keys">
          Ask one of them for a key with the access you need.
        </Notice>
      </Card>
    );
  }

  return (
    <Card
      title="API keys"
      padded={false}
      actions={
        <Button size="sm" variant="primary" icon={<Plus className="size-3.5" />} onClick={() => setCreating(true)}>
          Create key
        </Button>
      }
    >
      <div className="overflow-x-auto">
        <Table className="rounded-none border-0 shadow-none">
          <thead>
            <tr>
              <Th>Name</Th>
              <Th>Access</Th>
              <Th className="hidden md:table-cell">Made by</Th>
              <Th className="hidden sm:table-cell">Expires</Th>
              <Th>Last used</Th>
              <Th className="w-10" />
            </tr>
          </thead>
          <tbody>
            {keys.isPending ? (
              <SkeletonRows cols={6} rows={2} />
            ) : rows.length === 0 ? (
              <EmptyRow cols={6}>
                <EmptyState icon={<KeyRound className="size-5" />} title="No API keys" description="Create a private key for your server, scripts or an AI agent." />
              </EmptyRow>
            ) : (
              rows.map((k) => (
                <tr key={k.id}>
                  <Td>
                    <div className="font-medium text-zinc-900">{k.name}</div>
                    <code className="font-mono text-xs text-zinc-500">{k.prefix}…</code>
                  </Td>
                  <Td>
                    <div className="flex flex-wrap gap-1">
                      {k.scopes.map((s) => (
                        <Badge key={s} tone={s === "full" ? "amber" : "gray"}>
                          {scopeLabel(s)}
                        </Badge>
                      ))}
                    </div>
                  </Td>
                  <Td className="hidden text-zinc-600 md:table-cell">{k.created_by ?? "—"}</Td>
                  <Td className="hidden sm:table-cell">{k.expired ? <Badge tone="red">Expired</Badge> : <span className="text-zinc-600">{k.expires_at ? formatDate(k.expires_at, account.timezone) : "Never"}</span>}</Td>
                  <Td className="text-zinc-600">{formatRelative(k.last_used_at)}</Td>
                  <Td>
                    <IconButton label={`Revoke ${k.name}`} onClick={() => setRevoking(k)} className="hover:text-red-600">
                      <Trash2 className="size-3.5" />
                    </IconButton>
                  </Td>
                </tr>
              ))
            )}
          </tbody>
        </Table>
      </div>
      {creating ? (
        <CreateKeyModal
          onClose={() => setCreating(false)}
          onCreated={(k) => {
            setCreating(false);
            setCreated(k);
          }}
        />
      ) : null}
      {created ? (
        <Modal
          title="Copy your new key"
          onClose={() => setCreated(null)}
          size="md"
          footer={
            <Button variant="primary" onClick={() => setCreated(null)}>
              I've saved it
            </Button>
          }
        >
          <div className="space-y-3">
            <div className="flex items-start gap-2 rounded-md bg-amber-50 px-3 py-2 text-sm text-amber-800">
              <AlertTriangle className="mt-0.5 size-4 shrink-0" />
              This is the only time you'll see this key. Store it somewhere safe. It can do what its access allows, and never more than you can.
            </div>
            <CopyField value={created.key} />
          </div>
        </Modal>
      ) : null}
      {revoking ? (
        <ConfirmDialog
          title={`Revoke "${revoking.name}"?`}
          body="Anything using this key stops working immediately."
          confirmLabel="Revoke key"
          danger
          onConfirm={() => {
            revoke.mutate(revoking.id);
            setRevoking(null);
          }}
          onClose={() => setRevoking(null)}
        />
      ) : null}
    </Card>
  );
}

function CreateKeyModal({ onClose, onCreated }: { onClose: () => void; onCreated: (k: CreatedScopedKey) => void }) {
  const create = useCreateScopedKey();
  const [mode, setMode] = useState<"full" | "read" | "custom">("full");
  const [access, setAccess] = useState<Record<string, Access>>({ audience: "read", messaging: "none", content: "none", analytics: "read", account: "none" });

  const scopes =
    mode === "full"
      ? ["full"]
      : mode === "read"
        ? ["read"]
        : Object.entries(access)
            .filter(([, v]) => v !== "none")
            .map(([k, v]) => (v === "read" ? `${k}:read` : k));

  const onSubmit = (e: FormEvent<HTMLFormElement>) => {
    e.preventDefault();
    const d = new FormData(e.currentTarget);
    const days = String(d.get("expires"));
    create.mutate(
      { name: String(d.get("name")).trim(), scopes, expires_in_days: days ? Number(days) : undefined },
      { onSuccess: onCreated },
    );
  };

  return (
    <Modal title="Create API key" onClose={onClose} size="md">
      <form onSubmit={onSubmit} className="space-y-4">
        <Field label="Name" hint="So you know what uses it.">
          <Input name="name" required autoFocus placeholder="Claude agent" maxLength={100} />
        </Field>
        <div className="space-y-2">
          <div className="text-xs font-medium text-zinc-700">Access</div>
          <Segmented
            value={mode}
            onChange={setMode}
            options={[
              { value: "full", label: "Full access" },
              { value: "read", label: "Read only" },
              { value: "custom", label: "Choose areas" },
            ]}
          />
          {mode === "custom" ? (
            <div className="divide-y divide-zinc-100 rounded-md border border-zinc-200">
              {areas.map((a) => (
                <div key={a.key} className="flex items-center justify-between gap-3 px-3 py-2">
                  <div className="min-w-0 text-sm">
                    <div className="font-medium text-zinc-900">{a.label}</div>
                    <div className="text-xs text-zinc-500">{a.hint}</div>
                  </div>
                  <Segmented
                    size="sm"
                    value={access[a.key]}
                    onChange={(v) => setAccess({ ...access, [a.key]: v })}
                    options={[
                      { value: "none", label: "None" },
                      { value: "read", label: "Read" },
                      { value: "write", label: "Read and change" },
                    ]}
                  />
                </div>
              ))}
            </div>
          ) : (
            <p className="text-xs text-zinc-500">{mode === "full" ? "Can do anything you can, except security settings." : "Can read everything you can, and change nothing."}</p>
          )}
        </div>
        <Field label="Expires">
          <Select name="expires" defaultValue="">
            <option value="">Never</option>
            <option value="7">In 7 days</option>
            <option value="30">In 30 days</option>
            <option value="90">In 90 days</option>
            <option value="365">In a year</option>
          </Select>
        </Field>
        <InlineError message={create.error ? errorMessage(create.error) : scopes.length === 0 ? "Give the key access to at least one area." : null} />
        <div className="flex justify-end gap-2">
          <Button variant="ghost" onClick={onClose}>
            Cancel
          </Button>
          <Button type="submit" variant="primary" loading={create.isPending} disabled={scopes.length === 0}>
            Create key
          </Button>
        </div>
      </form>
    </Modal>
  );
}
