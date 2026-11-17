import { useState, type FormEvent } from "react";
import { useNavigate, useSearchParams } from "react-router";
import { Plug, Plus, Sparkles } from "lucide-react";
import { errorMessage } from "../../api/client";
import { Button, Checkbox, EmptyRow, EmptyState, Field, InlineError, Input, Modal, Page, PageHeader, RowLink, SkeletonRows, Table, Td, Th } from "../../components/ui";
import { formatNumber, formatRelative } from "../../lib/format";
import { useCreateFromRecipe, useCreateIntegration, useIntegrations, useRecipes } from "./api";
import { IntegrationStatusBadge } from "./parts";
import type { Recipe } from "./types";

const STARTER_SCRIPT = `// Turn each webhook into actions. input.payload is the body.
function handle(input) {
  var p = input.payload || {};
  if (!p.email) return [{ type: "log", message: "no email" }];
  return [
    { type: "upsert_profile", email: p.email, first_name: p.first_name },
    { type: "track_event", metric: "Signed Up", profile: { email: p.email }, unique_id: "myapp:" + p.id },
  ];
}
`;

export function IntegrationsPage() {
  const integrations = useIntegrations();
  const [params, setParams] = useSearchParams();
  const adding = params.get("add") === "1";
  const setAdding = (on: boolean) => {
    const next = new URLSearchParams(params);
    if (on) next.set("add", "1");
    else next.delete("add");
    setParams(next, { replace: true });
  };
  const rows = integrations.data ?? [];

  return (
    <Page>
      <PageHeader
        title="Integrations"
        description="Bring in bookings, orders and sign-ups from any app, by webhook or on a schedule."
        actions={
          <Button variant="primary" icon={<Plus className="size-4" />} onClick={() => setAdding(true)}>
            Add integration
          </Button>
        }
      />
      <Table>
        <thead>
          <tr>
            <Th>Integration</Th>
            <Th>Status</Th>
            <Th className="text-right">Delivered</Th>
            <Th className="text-right">Failed</Th>
            <Th>Last activity</Th>
          </tr>
        </thead>
        <tbody>
          {integrations.isPending ? (
            <SkeletonRows cols={5} />
          ) : integrations.isError ? (
            <EmptyRow cols={5}>
              <div className="py-6">
                <InlineError message={errorMessage(integrations.error)} />
              </div>
            </EmptyRow>
          ) : rows.length === 0 ? (
            <EmptyRow cols={5}>
              <EmptyState
                icon={<Plug className="size-5" />}
                title="No integrations yet"
                description="Start from Guesty, Stripe or Calendly, or connect any other app. You can also ask Claude to build one for you through the MCP server: just say which app."
                action={
                  <Button variant="primary" onClick={() => setAdding(true)}>
                    Add integration
                  </Button>
                }
              />
            </EmptyRow>
          ) : (
            rows.map((i) => (
              <RowLink key={i.id} to={`/integrations/${i.id}`}>
                <Td>
                  <div className="font-medium text-zinc-900">{i.name}</div>
                  <div className="text-xs text-zinc-500">{[i.recipe ? `From the ${i.recipe} recipe` : "Custom", i.has_pull ? "webhooks and pulls" : "webhooks"].join(" · ")}</div>
                </Td>
                <Td>
                  <IntegrationStatusBadge status={i.status} error={i.last_error} />
                </Td>
                <Td className="text-right tabular-nums">{formatNumber(i.deliveries_ok)}</Td>
                <Td className="text-right tabular-nums">{formatNumber(i.deliveries_failed)}</Td>
                <Td className="text-zinc-600">{i.last_delivery_at ? formatRelative(i.last_delivery_at) : "Nothing yet"}</Td>
              </RowLink>
            ))
          )}
        </tbody>
      </Table>
      {adding ? <AddIntegration onClose={() => setAdding(false)} /> : null}
    </Page>
  );
}

function AddIntegration({ onClose }: { onClose: () => void }) {
  const recipes = useRecipes();
  const [picked, setPicked] = useState<Recipe | "custom" | null>(null);
  return (
    <Modal title={picked === null ? "Add an integration" : picked === "custom" ? "Custom integration" : `Connect ${picked.name}`} onClose={onClose} size="lg">
      {picked === null ? (
        <div className="space-y-3">
          <p className="text-sm text-zinc-600">Start from a recipe, or build your own for any app that can send webhooks or has an API.</p>
          <div className="grid gap-2 sm:grid-cols-2">
            {(recipes.data ?? []).map((r) => (
              <button key={r.key} type="button" onClick={() => setPicked(r)} className="rounded-lg border border-zinc-200 p-3 text-left transition-colors hover:border-accent-600 hover:bg-accent-50/40">
                <div className="flex items-center justify-between gap-2">
                  <span className="font-medium text-zinc-900">{r.name}</span>
                  {r.category ? <span className="text-[11px] text-zinc-500">{r.category}</span> : null}
                </div>
                <p className="mt-1 line-clamp-3 text-xs text-zinc-600">{r.description}</p>
              </button>
            ))}
            <button type="button" onClick={() => setPicked("custom")} className="rounded-lg border border-dashed border-zinc-300 p-3 text-left transition-colors hover:border-accent-600">
              <div className="font-medium text-zinc-900">Any other app</div>
              <p className="mt-1 text-xs text-zinc-600">Get a webhook URL and write a short script that turns each payload into profiles and events.</p>
            </button>
          </div>
          <div className="flex items-start gap-2 rounded-md bg-zinc-50 p-3 text-xs text-zinc-600">
            <Sparkles className="mt-0.5 size-3.5 shrink-0 text-accent-700" />
            <span>Using Claude with DumbMail's MCP server? Ask it to "connect my Guesty" or "build an integration for my booking app"; it can do every step here.</span>
          </div>
        </div>
      ) : picked === "custom" ? (
        <CustomForm onBack={() => setPicked(null)} />
      ) : (
        <RecipeForm recipe={picked} onBack={() => setPicked(null)} />
      )}
    </Modal>
  );
}

function CustomForm({ onBack }: { onBack: () => void }) {
  const create = useCreateIntegration();
  const navigate = useNavigate();
  const onSubmit = (e: FormEvent<HTMLFormElement>) => {
    e.preventDefault();
    const name = String(new FormData(e.currentTarget).get("name")).trim();
    create.mutate({ name, script: STARTER_SCRIPT }, { onSuccess: (i) => navigate(`/integrations/${i.id}`) });
  };
  return (
    <form onSubmit={onSubmit} className="space-y-4">
      <Field label="Name">
        <Input name="name" required maxLength={100} placeholder="My booking app" autoFocus />
      </Field>
      <p className="text-sm text-zinc-600">You'll get a secret webhook URL and a starter script to edit and test.</p>
      <InlineError message={create.error ? errorMessage(create.error) : null} />
      <div className="flex justify-between gap-2">
        <Button variant="ghost" onClick={onBack}>
          Back
        </Button>
        <Button variant="primary" type="submit" loading={create.isPending}>
          Create
        </Button>
      </div>
    </form>
  );
}

function RecipeForm({ recipe, onBack }: { recipe: Recipe; onBack: () => void }) {
  const create = useCreateFromRecipe();
  const navigate = useNavigate();
  const [flows, setFlows] = useState(true);
  const onSubmit = (e: FormEvent<HTMLFormElement>) => {
    e.preventDefault();
    const name = String(new FormData(e.currentTarget).get("name")).trim();
    create.mutate({ recipe: recipe.key, name: name || undefined, create_flows: flows }, { onSuccess: (r) => navigate(`/integrations/${r.integration.id}`) });
  };
  return (
    <form onSubmit={onSubmit} className="space-y-4">
      <p className="text-sm text-zinc-600">{recipe.description}</p>
      <Field label="Name">
        <Input name="name" maxLength={100} placeholder={recipe.name} />
      </Field>
      <div>
        <div className="mb-1.5 text-xs font-medium text-zinc-700">Setup steps</div>
        <ol className="list-decimal space-y-1 pl-5 text-sm text-zinc-700">
          {recipe.setup.map((s) => (
            <li key={s}>{s}</li>
          ))}
        </ol>
      </div>
      {recipe.flows && recipe.flows.length > 0 ? (
        <Checkbox
          checked={flows}
          onChange={(e) => setFlows(e.target.checked)}
          label={`Also create ${recipe.flows.length === 1 ? "its suggested flow" : `its ${recipe.flows.length} suggested flows`} as drafts`}
          description={recipe.flows.map((f) => f.name).join(", ") + ". Nothing sends until you turn a flow on."}
        />
      ) : null}
      <InlineError message={create.error ? errorMessage(create.error) : null} />
      <div className="flex justify-between gap-2">
        <Button variant="ghost" onClick={onBack}>
          Back
        </Button>
        <Button variant="primary" type="submit" loading={create.isPending}>
          Create integration
        </Button>
      </div>
    </form>
  );
}
