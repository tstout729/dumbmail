import { useState, type FormEvent } from "react";
import { Link, useNavigate, useSearchParams } from "react-router";
import { LayoutTemplate, Mail, MessageSquare, Plus } from "lucide-react";
import type { Channel, EmailContent, SmsContent } from "../../api";
import { useCreateTemplate, useSession, useTemplates } from "../../api";
import { starterEmail } from "../../components/email/blocks";
import { EmailThumbnail } from "../../components/email/EmailPreview";
import { Button, EmptyState, Field, Input, Modal, Page, PageHeader, Segmented, Skeleton, Tabs, cx } from "../../components/ui";
import { formatRelative } from "../../lib/format";

type Tab = "all" | Channel;

export function TemplatesPage() {
  const templates = useTemplates();
  const [params, setParams] = useSearchParams();
  const [tab, setTab] = useState<Tab>("all");
  const creating = params.get("create") === "1";
  const rows = (templates.data ?? []).filter((t) => tab === "all" || t.channel === tab);

  const setCreating = (on: boolean) => {
    const next = new URLSearchParams(params);
    if (on) next.set("create", "1");
    else next.delete("create");
    setParams(next, { replace: true });
  };

  return (
    <Page>
      <PageHeader
        title="Templates"
        description="Reusable emails and texts for campaigns and flows."
        actions={
          <Button variant="primary" icon={<Plus className="size-4" />} onClick={() => setCreating(true)}>
            Create template
          </Button>
        }
      />
      <Tabs
        value={tab}
        onChange={setTab}
        tabs={[
          { value: "all", label: "All" },
          { value: "email", label: "Email" },
          { value: "sms", label: "SMS" },
        ]}
      />
      {templates.isPending ? (
        <div className="grid gap-4 sm:grid-cols-2 lg:grid-cols-3 xl:grid-cols-4">
          {[0, 1, 2, 3].map((i) => (
            <Skeleton key={i} className="h-60" />
          ))}
        </div>
      ) : rows.length === 0 ? (
        <div className="rounded-lg border border-zinc-200 bg-white">
          <EmptyState
            icon={<LayoutTemplate className="size-5" />}
            title="No templates yet"
            description="Design once, reuse everywhere. You can also save any campaign email as a template."
            action={
              <Button variant="primary" onClick={() => setCreating(true)}>
                Create template
              </Button>
            }
          />
        </div>
      ) : (
        <div className="grid gap-4 sm:grid-cols-2 lg:grid-cols-3 xl:grid-cols-4">
          {rows.map((t) => (
            <Link
              key={t.id}
              to={`/content/templates/${t.id}`}
              className="group overflow-hidden rounded-lg border border-zinc-200 bg-white shadow-xs transition-shadow hover:border-zinc-300 hover:shadow-md"
            >
              {t.channel === "email" ? (
                <EmailThumbnail content={t.content as EmailContent} />
              ) : (
                <div className="flex h-44 items-center justify-center bg-zinc-50 p-4">
                  <div className="max-w-[85%] line-clamp-6 rounded-2xl rounded-bl-md bg-zinc-200/80 px-3 py-2 text-xs text-zinc-800">
                    {(t.content as SmsContent).body || "Empty message"}
                  </div>
                </div>
              )}
              <div className="flex items-center gap-2 border-t border-zinc-100 px-3 py-2.5">
                {t.channel === "email" ? <Mail className="size-3.5 text-zinc-400" /> : <MessageSquare className="size-3.5 text-zinc-400" />}
                <span className="min-w-0 flex-1">
                  <span className="block truncate text-sm font-medium text-zinc-900 group-hover:text-accent-800">{t.name}</span>
                  <span className="block text-[11px] text-zinc-500">Edited {formatRelative(t.updated_at)}</span>
                </span>
              </div>
            </Link>
          ))}
        </div>
      )}
      {creating ? <CreateTemplateDialog onClose={() => setCreating(false)} /> : null}
    </Page>
  );
}

function CreateTemplateDialog({ onClose }: { onClose: () => void }) {
  const { account } = useSession();
  const [channel, setChannel] = useState<Channel>("email");
  const create = useCreateTemplate();
  const navigate = useNavigate();
  const onSubmit = (e: FormEvent<HTMLFormElement>) => {
    e.preventDefault();
    const name = String(new FormData(e.currentTarget).get("name")).trim();
    create.mutate(
      { name, channel, content: channel === "email" ? starterEmail(account.brand_color) : { body: "" } },
      { onSuccess: (t) => navigate(`/content/templates/${t.id}`) },
    );
  };
  return (
    <Modal title="Create template" onClose={onClose} size="sm">
      <form onSubmit={onSubmit} className="space-y-4">
        <Segmented
          value={channel}
          onChange={setChannel}
          options={[
            { value: "email", label: <><Mail className="size-3.5" /> Email</> },
            { value: "sms", label: <><MessageSquare className="size-3.5" /> SMS</> },
          ]}
        />
        <Field label="Name">
          <Input name="name" required autoFocus placeholder="Monthly newsletter" />
        </Field>
        <div className={cx("flex justify-end gap-2")}>
          <Button variant="ghost" onClick={onClose}>
            Cancel
          </Button>
          <Button type="submit" variant="primary" loading={create.isPending}>
            Create and edit
          </Button>
        </div>
      </form>
    </Modal>
  );
}
