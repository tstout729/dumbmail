import { useState, type FormEvent } from "react";
import { Button, Card, Field, InlineError, Skeleton, TagInput, Toggle } from "../../components/ui";
import { useLinkDefaults, useUpdateLinkDefaults } from "./api";
import { LinkParamsFields } from "./LinkParamsFields";
import type { LinkDefaults } from "./types";

/** Settings → Tracking: UTM parameters on the links in campaigns and flows. */
export function TrackingSettingsPage() {
  const defaults = useLinkDefaults();
  if (defaults.isPending) return <Skeleton className="h-64 w-full" />;
  if (defaults.error) return <InlineError message={defaults.error.message} />;
  return <TrackingForm key={JSON.stringify(defaults.data)} defaults={defaults.data} />;
}

function TrackingForm({ defaults }: { defaults: LinkDefaults }) {
  const update = useUpdateLinkDefaults();
  const [enabled, setEnabled] = useState(defaults.enabled);
  const [params, setParams] = useState(defaults.params);
  const [skip, setSkip] = useState(defaults.skip_domains);
  const dirty = enabled !== defaults.enabled || JSON.stringify(params) !== JSON.stringify(defaults.params) || JSON.stringify(skip) !== JSON.stringify(defaults.skip_domains);
  const onSubmit = (e: FormEvent<HTMLFormElement>) => {
    e.preventDefault();
    update.mutate({ enabled, params: { ...params, custom: params.custom.filter((c) => c.name || c.value) }, skip_domains: skip });
  };
  return (
    <form onSubmit={onSubmit} className="space-y-4">
      <Card title="Link tracking (UTM)">
        <div className="flex items-start justify-between gap-4">
          <p className="text-sm text-zinc-600">
            Add UTM parameters to the web links in your campaign and flow emails, so Google Analytics and similar tools can tell which email brought a visitor. Links
            that already have a parameter keep their own value. Unsubscribe and other DumbMail links are never changed. Links in texts are tagged when they're shortened (Settings → Texting).
          </p>
          <Toggle checked={enabled} onChange={setEnabled} label="Tag links" />
        </div>
        <p className="mt-2 text-xs text-zinc-500">
          {enabled ? "On for every campaign and flow, unless one turns it off." : "Off. A campaign or flow can still turn it on with its own values."} Campaigns get the
          tags in force when they start sending.
        </p>
      </Card>
      <Card title="Default values">
        <LinkParamsFields value={params} onChange={setParams} variables={defaults.variables} fallbacks={{ source: defaults.fallbacks.source, medium: defaults.fallbacks.medium, campaign: "the campaign or flow name and the start of its id" }} />
      </Card>
      <Card title="Domains not to tag">
        <Field label="Domains" hint="Links to these domains and their subdomains are left as they are. Press Enter after each one. Up to 50.">
          <TagInput value={skip} onChange={setSkip} placeholder="checkout.example.com" aria-label="Domains not to tag" />
        </Field>
      </Card>
      <InlineError message={update.error?.message} />
      <div className="flex justify-end">
        <Button type="submit" variant="primary" loading={update.isPending} disabled={!dirty}>
          Save
        </Button>
      </div>
    </form>
  );
}
