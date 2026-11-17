import { useState } from "react";
import { Link } from "react-router";
import { Card, CodeBlock, Segmented } from "../../components/ui";
import { ApiKeysCard } from "../../features/team/ApiKeysCard";

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
            Let Claude, Cursor or any MCP client run your marketing: point it at <code className="rounded bg-zinc-100 px-1 text-xs">{origin}/mcp</code> with a private API key. Give each key only the access it needs.
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
