import { useState } from "react";
import { useNavigate, useParams, useSearchParams } from "react-router";
import { AlertTriangle, RotateCw, Trash2 } from "lucide-react";
import { errorMessage } from "../../api/client";
import { useSession } from "../../api";
import { Button, Card, Checkbox, ConfirmDialog, CopyField, InlineError, Notice, Page, PageHeader, Skeleton, Stat, Tabs, Toggle } from "../../components/ui";
import { formatDateTime, formatNumber, formatRelative } from "../../lib/format";
import { toast } from "../../lib/toast";
import { useDeleteIntegration, useIntegration, useRecipes, useRotateIntegrationUrl, useSetIntegrationStatus, useUpdateIntegration } from "./api";
import { DeliveriesTab } from "./DeliveriesTab";
import { IntegrationStatusBadge } from "./parts";
import { PullTab } from "./PullTab";
import { ScriptTab } from "./ScriptTab";
import { SignatureCard } from "./SignatureCard";
import type { Integration } from "./types";

type Tab = "setup" | "script" | "pull" | "deliveries";
const TABS: Tab[] = ["setup", "script", "pull", "deliveries"];

export function IntegrationPage() {
  const { id = "" } = useParams();
  const q = useIntegration(id);
  const [params, setParams] = useSearchParams();
  const tabParam = params.get("tab");
  const tab: Tab = TABS.find((t) => t === tabParam) ?? "setup";
  const setTab = (t: Tab) => {
    const next = new URLSearchParams(params);
    if (t === "setup") next.delete("tab");
    else next.set("tab", t);
    setParams(next, { replace: true });
  };

  if (q.isPending) {
    return (
      <Page>
        <Skeleton className="mb-4 h-7 w-64" />
        <Skeleton className="h-48 w-full" />
      </Page>
    );
  }
  if (q.isError) {
    return (
      <Page>
        <PageHeader title="Integration" breadcrumbs={[{ label: "Integrations", to: "/integrations" }]} />
        <InlineError message={errorMessage(q.error)} />
      </Page>
    );
  }
  const i = q.data;
  return (
    <Page>
      <PageHeader title={i.name} breadcrumbs={[{ label: "Integrations", to: "/integrations" }]} description={<IntegrationStatusBadge status={i.status} error={i.last_error} />} actions={<HeaderActions integration={i} />} />
      <Problems integration={i} onTab={setTab} />
      <Tabs
        value={tab}
        onChange={setTab}
        tabs={[
          { value: "setup", label: "Setup" },
          { value: "script", label: "Script" },
          { value: "pull", label: "Pull" },
          { value: "deliveries", label: "Delivery log" },
        ]}
      />
      {tab === "setup" ? <SetupTab integration={i} /> : null}
      {tab === "script" ? <ScriptTab key={i.updated_at} integration={i} /> : null}
      {tab === "pull" ? <PullTab key={i.updated_at} integration={i} /> : null}
      {tab === "deliveries" ? <DeliveriesTab integration={i} /> : null}
    </Page>
  );
}

function HeaderActions({ integration: i }: { integration: Integration }) {
  const status = useSetIntegrationStatus(i.id);
  const remove = useDeleteIntegration();
  const navigate = useNavigate();
  const [deleting, setDeleting] = useState(false);
  return (
    <>
      <div className="flex items-center gap-2 text-sm text-zinc-700">
        <Toggle checked={i.status === "enabled"} disabled={status.isPending} onChange={(on) => status.mutate(on ? "enabled" : "disabled", { onError: (e) => toast.error(errorMessage(e)) })} label="Turn integration on or off" />
        {i.status === "enabled" ? "On" : "Off"}
      </div>
      <Button variant="ghost" icon={<Trash2 className="size-4" />} onClick={() => setDeleting(true)}>
        Delete
      </Button>
      {deleting ? (
        <ConfirmDialog
          title={`Delete ${i.name}?`}
          body="The webhook URL stops working and the delivery log is deleted. Profiles and events it already wrote stay."
          confirmLabel="Delete integration"
          danger
          loading={remove.isPending}
          onClose={() => setDeleting(false)}
          onConfirm={() =>
            remove.mutate(i.id, {
              onSuccess: () => {
                toast.success("Integration deleted");
                navigate("/integrations");
              },
              onError: (e) => toast.error(errorMessage(e)),
            })
          }
        />
      ) : null}
    </>
  );
}

function Problems({ integration: i, onTab }: { integration: Integration; onTab: (t: Tab) => void }) {
  const items = [];
  if (i.status === "disabled") {
    items.push(
      <Notice key="off" tone="gray" title="This integration is off">
        Webhooks are kept in the delivery log but nothing is written, and scheduled pulls are paused. Turn it on, then replay what arrived.
      </Notice>,
    );
  }
  if (!i.signature_ready) {
    items.push(
      <Notice key="sig" tone="amber" icon={<AlertTriangle />} title="Add the signing secret">
        Webhooks are refused until the secret for the signature check is saved below.
      </Notice>,
    );
  }
  if (i.missing_credentials.length > 0) {
    items.push(
      <Notice key="creds" tone="amber" icon={<AlertTriangle />} title={`Add credentials: ${i.missing_credentials.join(", ")}`}>
        The pull needs them.{" "}
        <button type="button" className="font-medium underline" onClick={() => onTab("pull")}>
          Open pull settings
        </button>
      </Notice>,
    );
  }
  if (i.last_error) {
    items.push(
      <Notice key="err" tone="red" icon={<AlertTriangle />} title={`Last error${i.last_error_at ? `, ${formatRelative(i.last_error_at)}` : ""}`}>
        <span className="break-words">{i.last_error}</span>{" "}
        <button type="button" className="font-medium underline" onClick={() => onTab("deliveries")}>
          See the delivery log
        </button>
      </Notice>,
    );
  }
  return items.length > 0 ? <div className="mb-4 space-y-2">{items}</div> : null;
}

function SetupTab({ integration: i }: { integration: Integration }) {
  const { account } = useSession();
  const rotate = useRotateIntegrationUrl(i.id);
  const recipes = useRecipes();
  const recipe = (recipes.data ?? []).find((r) => r.key === i.recipe);
  const [rotating, setRotating] = useState(false);
  return (
    <div className="space-y-4">
      <div className="grid gap-3 sm:grid-cols-3">
        <Stat label="Delivered" value={formatNumber(i.deliveries_ok)} />
        <Stat label="Failed" value={formatNumber(i.deliveries_failed)} />
        <Stat label="Last activity" value={i.last_delivery_at ? formatRelative(i.last_delivery_at) : "None yet"} sub={i.last_delivery_at ? formatDateTime(i.last_delivery_at, account.timezone) : undefined} />
      </div>
      <Card
        title="Webhook URL"
        actions={
          <Button size="sm" variant="ghost" icon={<RotateCw className="size-3.5" />} onClick={() => setRotating(true)}>
            New URL
          </Button>
        }
      >
        <p className="mb-2 text-sm text-zinc-600">Have the app send webhooks (POST requests) here. Anyone with this URL can send data, so keep it private.</p>
        <CopyField value={i.inbound_url} />
      </Card>
      <SignatureCard key={i.updated_at} integration={i} secretHelp={recipe?.signature_secret ?? null} />
      <ConsentCard integration={i} />
      {recipe ? (
        <Card title={`${recipe.name} setup`}>
          <ol className="list-decimal space-y-1.5 pl-5 text-sm text-zinc-700">
            {recipe.setup.map((s) => (
              <li key={s} className="break-words">
                {s}
              </li>
            ))}
          </ol>
          {recipe.docs && recipe.docs.length > 0 ? (
            <div className="mt-3 text-xs text-zinc-500">
              Docs:{" "}
              {recipe.docs.map((d, n) => (
                <span key={d}>
                  {n > 0 ? ", " : ""}
                  <a href={d} target="_blank" rel="noreferrer" className="break-all text-accent-700 hover:underline">
                    {new URL(d).hostname.replace(/^www\./, "")}
                    {new URL(d).pathname.length > 1 ? new URL(d).pathname : ""}
                  </a>
                </span>
              ))}
            </div>
          ) : null}
        </Card>
      ) : null}
      {rotating ? (
        <ConfirmDialog
          title="Make a new webhook URL?"
          body="The current URL stops working right away. Give the app the new one."
          confirmLabel="Make a new URL"
          loading={rotate.isPending}
          onClose={() => setRotating(false)}
          onConfirm={() =>
            rotate.mutate(undefined, {
              onSuccess: () => {
                setRotating(false);
                toast.success("New URL made. Update it in the app.");
              },
              onError: (e) => toast.error(errorMessage(e)),
            })
          }
        />
      ) : null}
    </div>
  );
}

function ConsentCard({ integration: i }: { integration: Integration }) {
  const update = useUpdateIntegration(i.id);
  const [confirming, setConfirming] = useState(false);
  const set = (on: boolean) =>
    update.mutate(
      { contacts_opted_in: on },
      {
        onSuccess: () => {
          setConfirming(false);
          toast.success(on ? "People it adds can get marketing" : "People it adds are held back from marketing");
        },
        onError: (e) => toast.error(errorMessage(e)),
      },
    );
  return (
    <Card title="Marketing consent">
      <p className="mb-3 text-sm text-zinc-600">
        People this integration adds are held back from marketing unless they opted in: campaigns skip them, and only flows marked as service messages (like a booking confirmation) reach them. Signing up through a form clears the hold.
      </p>
      <Checkbox
        checked={i.contacts_opted_in}
        disabled={update.isPending}
        onChange={(e) => (e.target.checked ? setConfirming(true) : set(false))}
        label="Everyone this integration adds has agreed to get marketing email from us."
        description="Only tick this if it's true for every person the app sends. It applies to people it adds from now on."
      />
      {confirming ? (
        <ConfirmDialog
          title="Did everyone agree to marketing email?"
          body="People this integration adds will get campaigns and every flow. Only confirm if each of them agreed to get marketing email from you."
          confirmLabel="Yes, they all agreed"
          loading={update.isPending}
          onClose={() => setConfirming(false)}
          onConfirm={() => set(true)}
        />
      ) : null}
    </Card>
  );
}
