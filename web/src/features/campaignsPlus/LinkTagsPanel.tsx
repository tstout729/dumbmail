import { useState } from "react";
import { Link2 } from "lucide-react";
import { Button, InlineError, Segmented, Skeleton } from "../../components/ui";
import { useLinkDefaults, useLinkTags, useSetLinkTags } from "./api";
import { LinkParamsFields, emptyParams } from "./LinkParamsFields";
import type { LinkOverride, LinkParams, LinkTags } from "./types";

type Mode = LinkOverride["mode"];

/** One line saying what a campaign's or flow's links get. */
export function LinkTagsLine({ tags }: { tags: LinkTags }) {
  return (
    <p className="flex items-start gap-1.5 text-xs text-zinc-600">
      <Link2 className="mt-0.5 size-3.5 shrink-0 text-zinc-400" />
      {tags.tagged && tags.preview ? (
        <span className="min-w-0">
          {tags.sent ? "Links were tagged:" : "Links will be tagged:"} <code className="break-all font-mono text-zinc-800">{tags.preview}</code>
        </span>
      ) : (
        <span>{tags.sent ? "Links weren't tagged." : "Links won't be tagged."}</span>
      )}
    </p>
  );
}

/** A campaign's or flow's link tagging: the account's defaults, off, or its own values. */
export function LinkTagsPanel({ kind, id, disabled }: { kind: "campaign" | "flow"; id: string; disabled?: boolean }) {
  const tags = useLinkTags(kind, id);
  const defaults = useLinkDefaults();
  if (tags.isPending || defaults.isPending) return <Skeleton className="h-16 w-full" />;
  if (tags.error) return <InlineError message={tags.error.message} />;
  return <Editor key={JSON.stringify(tags.data.override)} kind={kind} id={id} tags={tags.data} variables={defaults.data?.variables ?? tags.data.variables} disabled={disabled} />;
}

function Editor({ kind, id, tags, variables, disabled }: { kind: "campaign" | "flow"; id: string; tags: LinkTags; variables: LinkTags["variables"]; disabled?: boolean }) {
  const set = useSetLinkTags(kind, id);
  const [mode, setMode] = useState<Mode>(tags.override.mode);
  const [params, setParams] = useState<LinkParams>(tags.override.mode === "custom" ? tags.override.params : emptyParams);
  const saved = tags.override.mode === mode && (mode !== "custom" || (tags.override.mode === "custom" && JSON.stringify(tags.override.params) === JSON.stringify(params)));
  const save = () => {
    const o: LinkOverride = mode === "custom" ? { mode, params: { ...params, custom: params.custom.filter((c) => c.name || c.value) } } : { mode };
    set.mutate(o);
  };
  return (
    <div className="space-y-3 rounded-lg border border-zinc-200 p-3">
      <div className="flex flex-wrap items-center justify-between gap-2">
        <div className="text-sm font-medium text-zinc-900">Link tracking (UTM)</div>
        {disabled ? null : (
          <Segmented<Mode>
            size="sm"
            value={mode}
            onChange={setMode}
            options={[
              { value: "default", label: tags.account_enabled ? "Account defaults" : "Account default (off)" },
              { value: "off", label: "Off" },
              { value: "custom", label: "Custom" },
            ]}
          />
        )}
      </div>
      {mode === "custom" && !disabled ? (
        <LinkParamsFields value={params} onChange={setParams} variables={variables} fallbacks={{ source: "dumbmail", medium: "{channel}", campaign: `the ${kind} name and the start of its id` }} />
      ) : null}
      {saved ? <LinkTagsLine tags={tags} /> : <p className="text-xs text-zinc-500">Save to see what the links get.</p>}
      {kind === "flow" && saved && tags.tagged ? <p className="text-xs text-zinc-500">Each step uses its own name for {"{message_name}"} and its channel for {"{channel}"}.</p> : null}
      <InlineError message={set.error?.message} />
      {!disabled && !saved ? (
        <div className="flex justify-end">
          <Button size="sm" variant="primary" loading={set.isPending} onClick={save}>
            Save link tracking
          </Button>
        </div>
      ) : null}
    </div>
  );
}
