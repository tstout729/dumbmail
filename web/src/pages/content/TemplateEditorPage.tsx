import { InlineTitle } from "../../components/ui/InlineTitle";
import { useState } from "react";
import { useBlocker, useNavigate, useParams } from "react-router";
import { Trash2 } from "lucide-react";
import type { EmailContent, SmsContent, Template } from "../../api";
import { useDeleteTemplate, useTemplate, useUpdateTemplate } from "../../api";
import { EmailEditor } from "../../components/email/EmailEditor";
import { SmsEditor } from "../../components/email/SmsEditor";
import { Badge, Button, ConfirmDialog, Page, PageHeader, Skeleton } from "../../components/ui";
import { toast } from "../../lib/toast";

export function TemplateEditorPage() {
  const { id = "" } = useParams();
  const template = useTemplate(id);
  if (template.isPending) {
    return (
      <Page wide>
        <Skeleton className="mb-6 h-7 w-64" />
        <Skeleton className="h-[480px] w-full" />
      </Page>
    );
  }
  if (template.error) {
    return (
      <Page>
        <PageHeader title="Template" breadcrumbs={[{ label: "Templates", to: "/content/templates" }]} />
        <p className="text-sm text-red-600">{template.error.message}</p>
      </Page>
    );
  }
  return <TemplateEditor key={template.data.id} template={template.data} />;
}

type Draft = Pick<Template, "name" | "content">;

function TemplateEditor({ template }: { template: Template }) {
  const update = useUpdateTemplate(template.id);
  const remove = useDeleteTemplate();
  const navigate = useNavigate();
  const [draft, setDraft] = useState<Draft | null>(null);
  const [confirmDelete, setConfirmDelete] = useState(false);
  const current = draft ?? { name: template.name, content: template.content };
  const dirty = draft !== null;
  const blocker = useBlocker(
    ({ currentLocation, nextLocation }) =>
      dirty && currentLocation.pathname !== nextLocation.pathname && !(nextLocation.state as { saved?: boolean } | null)?.saved,
  );

  const save = () => {
    if (!draft) return;
    update.mutate(draft, {
      onSuccess: () => {
        setDraft(null);
        toast.success("Template saved");
      },
    });
  };

  return (
    <Page wide>
      <PageHeader
        breadcrumbs={[{ label: "Templates", to: "/content/templates" }]}
        title={
          <span className="flex items-center gap-2">
            <InlineTitle
              value={current.name}
              onChange={(e) => setDraft({ ...current, name: e.target.value })}
              aria-label="Template name"
            />
            <Badge>{template.channel === "email" ? "Email" : "SMS"}</Badge>
          </span>
        }
        actions={
          <>
            <Button variant="ghost" icon={<Trash2 className="size-3.5" />} onClick={() => setConfirmDelete(true)}>
              Delete
            </Button>
            <Button variant={dirty ? "primary" : "secondary"} disabled={!dirty} loading={update.isPending} onClick={save}>
              {dirty ? "Save" : "Saved"}
            </Button>
          </>
        }
      />
      {template.channel === "email" ? (
        <EmailEditor value={current.content as EmailContent} onChange={(content) => setDraft({ ...current, content })} templates={false} />
      ) : (
        <SmsEditor value={current.content as SmsContent} onChange={(content) => setDraft({ ...current, content })} templates={false} />
      )}
      {confirmDelete ? (
        <ConfirmDialog
          title="Delete this template?"
          body="Campaigns and flows that already used it keep their copy."
          confirmLabel="Delete template"
          danger
          loading={remove.isPending}
          onConfirm={() => remove.mutate(template.id, { onSuccess: () => navigate("/content/templates", { state: { saved: true } }) })}
          onClose={() => setConfirmDelete(false)}
        />
      ) : null}
      {blocker.state === "blocked" ? (
        <ConfirmDialog
          title="Leave without saving?"
          body="Your changes to this template haven't been saved."
          confirmLabel="Discard changes"
          danger
          onConfirm={() => blocker.proceed()}
          onClose={() => blocker.reset()}
        />
      ) : null}
    </Page>
  );
}
