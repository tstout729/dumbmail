import { useState, type FormEvent } from "react";
import { Link } from "react-router";
import { AlertTriangle, KeyRound, Plus, Trash2 } from "lucide-react";
import type { ApiKey, CreatedApiKey } from "../../api";
import { useApiKeys, useCreateApiKey, useRevokeApiKey, useSession } from "../../api";
import {
  Button,
  Card,
  CodeBlock,
  ConfirmDialog,
  CopyField,
  EmptyRow,
  EmptyState,
  Field,
  IconButton,
  Input,
  Modal,
  Segmented,
  SkeletonRows,
  Table,
  Td,
  Th,
} from "../../components/ui";
import { formatDate, formatRelative } from "../../lib/format";

const mcpAbilities = [
  "Look up, create and update profiles; import contacts",
  "Build and preview segments with the same rules as the UI",
  "Draft campaigns, estimate reach, send tests, schedule or send",
  "Create flows from blueprints and turn them on or off",
  "Write and render email and SMS content and templates",
  "Track events and query metrics and revenue analytics",
];

export function DevelopersPage() {
  const origin = window.location.origin;
  const [client, setClient] = useState<"code" | "desktop" | "cursor">("code");

  const configs = {
    code: { label: "Terminal", code: `claude mcp add --transport http dumbmail ${origin}/mcp --header "Authorization: Bearer <key>"` },
    desktop: {
      label: "claude_desktop_config.json",
      code: JSON.stringify({ mcpServers: { dumbmail: { command: "npx", args: ["-y", "mcp-remote", `${origin}/mcp`, "--header", "Authorization: Bearer <key>"] } } }, null, 2),
    },
    cursor: {
      label: ".cursor/mcp.json",
      code: JSON.stringify({ mcpServers: { dumbmail: { url: `${origin}/mcp`, headers: { Authorization: "Bearer <key>" } } } }, null, 2),
    },
  };

  const curl = `curl -X POST ${origin}/api/events \\
  -H "Authorization: Bearer <key>" \\
  -H "Content-Type: application/json" \\
  -d '{
    "metric": "Placed Order",
    "profile": { "email": "jane@example.com", "first_name": "Jane" },
    "properties": { "order_id": "1001", "items": [{ "name": "Mug", "price": 18 }] },
    "value": 18,
    "unique_id": "order-1001"
  }'`;

  return (
    <div className="space-y-4">
      <p className="text-sm text-zinc-500">
        API keys, the MCP server for AI agents, and server-side events. The website snippet is under{" "}
        <Link to="/settings/website" className="font-medium text-accent-700 hover:underline">
          Website
        </Link>
        .
      </p>
      <div className="space-y-4">
        <ApiKeysCard />
        <Card title="MCP server">
          <p className="mb-3 text-sm text-zinc-600">
            Let Claude, Cursor or any MCP client run your marketing: point it at <code className="rounded bg-zinc-100 px-1 text-xs">{origin}/mcp</code> with a private API key.
          </p>
          <div className="mb-3">
            <Segmented
              value={client}
              onChange={setClient}
              options={[
                { value: "code", label: "Claude Code" },
                { value: "desktop", label: "Claude Desktop" },
                { value: "cursor", label: "Cursor" },
              ]}
            />
          </div>
          <CodeBlock code={configs[client].code} label={configs[client].label} />
          <p className="mt-2 text-xs text-zinc-500">Replace &lt;key&gt; with a key from above.</p>
          <div className="mt-4">
            <div className="mb-2 text-xs font-semibold uppercase tracking-wider text-zinc-400">What agents can do</div>
            <ul className="grid gap-1.5 text-sm text-zinc-700 sm:grid-cols-2">
              {mcpAbilities.map((a) => (
                <li key={a} className="flex gap-2">
                  <span className="mt-1.5 size-1.5 shrink-0 rounded-full bg-accent-500" />
                  {a}
                </li>
              ))}
            </ul>
          </div>
        </Card>
        <Card title="Send events from your server">
          <p className="mb-3 text-sm text-zinc-600">
            Server-side events (orders, sign-ups, anything) trigger flows and power segments. Use <code className="rounded bg-zinc-100 px-1 text-xs">unique_id</code> to make retries safe.
          </p>
          <CodeBlock code={curl} label="curl" />
        </Card>
      </div>
    </div>
  );
}

function ApiKeysCard() {
  const { account } = useSession();
  const keys = useApiKeys();
  const create = useCreateApiKey();
  const revoke = useRevokeApiKey();
  const [creating, setCreating] = useState(false);
  const [created, setCreated] = useState<CreatedApiKey | null>(null);
  const [revoking, setRevoking] = useState<ApiKey | null>(null);

  const onCreate = (e: FormEvent<HTMLFormElement>) => {
    e.preventDefault();
    const name = String(new FormData(e.currentTarget).get("name")).trim();
    create.mutate(name, {
      onSuccess: (k) => {
        setCreating(false);
        setCreated(k);
      },
    });
  };

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
      <Table className="rounded-none border-0 shadow-none">
        <thead>
          <tr>
            <Th>Name</Th>
            <Th>Key</Th>
            <Th>Created</Th>
            <Th>Last used</Th>
            <Th className="w-10" />
          </tr>
        </thead>
        <tbody>
          {keys.isPending ? (
            <SkeletonRows cols={5} rows={2} />
          ) : (keys.data ?? []).length === 0 ? (
            <EmptyRow cols={5}>
              <EmptyState icon={<KeyRound className="size-5" />} title="No API keys" description="Create a private key for your server, scripts or an AI agent." />
            </EmptyRow>
          ) : (
            (keys.data ?? []).map((k) => (
              <tr key={k.id}>
                <Td className="font-medium text-zinc-900">{k.name}</Td>
                <Td>
                  <code className="font-mono text-xs text-zinc-600">{k.prefix}…</code>
                </Td>
                <Td className="text-zinc-600">{formatDate(k.created_at, account.timezone)}</Td>
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
      {creating ? (
        <Modal title="Create API key" onClose={() => setCreating(false)} size="sm">
          <form onSubmit={onCreate} className="space-y-4">
            <Field label="Name" hint="So you know what uses it.">
              <Input name="name" required autoFocus placeholder="Claude agent" />
            </Field>
            <div className="flex justify-end gap-2">
              <Button variant="ghost" onClick={() => setCreating(false)}>
                Cancel
              </Button>
              <Button type="submit" variant="primary" loading={create.isPending}>
                Create key
              </Button>
            </div>
          </form>
        </Modal>
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
              This is the only time you'll see this key. Store it somewhere safe; it has full access to your account.
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
