import { Link } from "react-router";
import { CheckCircle2, CircleDashed, ExternalLink } from "lucide-react";
import type { Form, FormKind } from "../../api";
import { useWebsite } from "../../api";
import { Card, CodeBlock, CopyField, Notice } from "../../components/ui";
import { formatRelative } from "../../lib/format";
import { isOverlayKind } from "./editorShared";
import { openSnippet } from "./TargetingSection";

export function InstallPanel({ form, kind }: { form: Form; kind: FormKind }) {
  const website = useWebsite();
  const installed = website.data?.installed ?? false;
  const hosted = `${window.location.origin}/f/${form.id}`;
  const draft = form.status !== "live";
  const snippetStatus = website.isPending ? null : installed ? (
    <Notice tone="green" icon={<CheckCircle2 />} title="Website snippet installed">
      Last seen {formatRelative(website.data?.last_seen_at)}.
    </Notice>
  ) : (
    <Notice tone="amber" icon={<CircleDashed />} title="Website snippet not detected yet">
      <Link to="/settings/website" className="font-medium underline">
        Add the snippet to your site
      </Link>{" "}
      once and every form can show there.
    </Notice>
  );
  return (
    <div className="grid grid-cols-1 gap-4 lg:grid-cols-2">
      {kind === "page" ? null : (
        <Card title="Show it on your site">
          <div className="space-y-3">
            {kind === "embed" ? (
              <>
                <p className="text-sm text-zinc-600">Put this where the form should appear. It renders there once the website snippet is installed.</p>
                <CodeBlock code={`<div data-dumbmail-form="${form.id}"></div>`} label="HTML" />
              </>
            ) : (
              <p className="text-sm text-zinc-600">
                Nothing to paste for this form. Once the{" "}
                <Link to="/settings/website" className="font-medium text-accent-700 hover:underline">
                  website snippet
                </Link>{" "}
                is installed, this {kind} appears on its own, following the targeting rules on the Design tab.
              </p>
            )}
            {snippetStatus}
            {draft ? <p className="text-xs text-amber-700">The form is a draft. Publish it before it shows on your site.</p> : null}
          </div>
        </Card>
      )}
      {isOverlayKind(kind) ? (
        <Card title="Open it from a button">
          <p className="mb-3 text-sm text-zinc-600">
            Any button or link with this attribute opens the {kind} when clicked, whatever its targeting. Use your own text and styling.
          </p>
          <CodeBlock code={openSnippet(form.id)} label="HTML" />
        </Card>
      ) : null}
      <Card title="Hosted sign-up page">
        <p className="mb-3 text-sm text-zinc-600">Share this link anywhere: bio links, QR codes, receipts.</p>
        <CopyField value={hosted} />
        <a href={hosted} target="_blank" rel="noreferrer" className="mt-3 inline-flex items-center gap-1 text-sm font-medium text-accent-700 hover:underline">
          Open page <ExternalLink className="size-3.5" />
        </a>
        {kind === "page" && draft ? <p className="mt-3 text-xs text-amber-700">The form is a draft. Publish it before sharing the link.</p> : null}
      </Card>
      {form.preview_url ? (
        <Card title="Preview link">
          <p className="mb-3 text-sm text-zinc-600">Preview, including drafts; sign-ups there aren&rsquo;t saved. Share it with your team to review before publishing.</p>
          <CopyField value={form.preview_url} />
          <a
            href={form.preview_url}
            target="_blank"
            rel="noreferrer"
            className="mt-3 inline-flex items-center gap-1 text-sm font-medium text-accent-700 hover:underline"
          >
            Open preview <ExternalLink className="size-3.5" />
          </a>
          <p className="mt-3 text-xs text-zinc-500">It shows the last saved version. Save first to see your latest changes.</p>
        </Card>
      ) : null}
    </div>
  );
}
