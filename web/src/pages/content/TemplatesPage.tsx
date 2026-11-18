import { useState, type FormEvent } from "react";
import { Link, useNavigate, useSearchParams } from "react-router";
import { Archive, ArchiveRestore, LayoutTemplate, Mail, MessageSquare, MoreHorizontal, Plus, Tag } from "lucide-react";
import type { Channel, EmailContent, SmsContent, Template } from "../../api";
import { useCreateTemplate, useSession } from "../../api";
import { ITEMS_PAGE, useArchive, useItems } from "../../features/campaignsPlus/api";
import { Pager, TagChips, TagsDialog } from "../../features/campaignsPlus/TagsDialog";
import type { TagRef } from "../../features/campaignsPlus/types";
import { starterEmail } from "../../components/email/blocks";
import { EmailThumbnail } from "../../components/email/EmailPreview";
import { Button, Checkbox, EmptyState, Field, IconButton, Input, Menu, Modal, Page, PageHeader, Segmented, Skeleton, Tabs, cx } from "../../components/ui";
import { formatRelative } from "../../lib/format";
import { useTemplateFromStarter } from "../../features/content/api";
import { StarterGallery } from "../../features/content/StarterGallery";

type Tab = "all" | Channel;

export function TemplatesPage() {
  const [showArchived, setShowArchived] = useState(false);
  const [offset, setOffset] = useState(0);
  const archive = useArchive("templates");
  const [tagging, setTagging] = useState<(Template & { tags: TagRef[] }) | null>(null);
  const [params, setParams] = useSearchParams();
  const [tab, setTab] = useState<Tab>("all");
  const page = useItems<Template>("templates", { archived: showArchived, channel: tab === "all" ? undefined : tab, offset });
  const templates = { ...page, data: page.data?.data };
  const creating = params.get("create") === "1";
  const rows = templates.data ?? [];

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
      <div className="flex flex-wrap items-start justify-between gap-3">
        <Tabs
          value={tab}
          onChange={(t) => {
            setTab(t);
            setOffset(0);
          }}
          tabs={[
            { value: "all", label: "All" },
            { value: "email", label: "Email" },
            { value: "sms", label: "SMS" },
          ]}
        />
        <Checkbox
          label="Archived"
          checked={showArchived}
          onChange={(e) => {
            setShowArchived(e.target.checked);
            setOffset(0);
          }}
          className="mt-1.5"
        />
      </div>
      {templates.isPending ? (
        <div className="grid grid-cols-1 gap-4 sm:grid-cols-2 lg:grid-cols-3 xl:grid-cols-4">
          {[0, 1, 2, 3].map((i) => (
            <Skeleton key={i} className="h-60" />
          ))}
        </div>
      ) : rows.length === 0 ? (
        <div className="rounded-lg border border-zinc-200 bg-white">
          <EmptyState
            icon={<LayoutTemplate className="size-5" />}
            title={showArchived ? "No archived templates" : "No templates yet"}
            description="Design once, reuse everywhere. You can also save any campaign email as a template."
            action={
              <Button variant="primary" onClick={() => setCreating(true)}>
                Create template
              </Button>
            }
          />
        </div>
      ) : (
        <div className="grid grid-cols-1 gap-4 sm:grid-cols-2 lg:grid-cols-3 xl:grid-cols-4">
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
                  <TagChips tags={t.tags} />
                </span>
                <span
                  onClick={(e) => {
                    e.preventDefault();
                    e.stopPropagation();
                  }}
                >
                  <Menu
                    trigger={(props) => (
                      <IconButton label="Template actions" size="sm" {...props}>
                        <MoreHorizontal className="size-4" />
                      </IconButton>
                    )}
                    items={[
                      { label: "Tags…", icon: <Tag className="size-3.5" />, onSelect: () => setTagging(t) },
                      {
                        label: t.archived_at ? "Unarchive" : "Archive",
                        icon: t.archived_at ? <ArchiveRestore className="size-3.5" /> : <Archive className="size-3.5" />,
                        onSelect: () => archive.mutate({ ids: [t.id], archived: !t.archived_at }),
                      },
                    ]}
                  />
                </span>
              </div>
            </Link>
          ))}
        </div>
      )}
      <Pager offset={offset} total={page.data?.total ?? 0} page={ITEMS_PAGE} onChange={setOffset} />
      {creating ? <CreateTemplateDialog onClose={() => setCreating(false)} /> : null}
      {tagging ? <TagsDialog kind="templates" id={tagging.id} name={tagging.name} tags={tagging.tags} onClose={() => setTagging(null)} /> : null}
    </Page>
  );
}

function CreateTemplateDialog({ onClose }: { onClose: () => void }) {
  const { account } = useSession();
  const [channel, setChannel] = useState<Channel>("email");
  const [starter, setStarter] = useState<string>("blank");
  const create = useCreateTemplate();
  const fromStarter = useTemplateFromStarter();
  const navigate = useNavigate();
  const onSubmit = (e: FormEvent<HTMLFormElement>) => {
    e.preventDefault();
    const name = String(new FormData(e.currentTarget).get("name")).trim();
    const open = { onSuccess: (t: { id: string }) => navigate(`/content/templates/${t.id}`) };
    if (channel === "email" && starter !== "blank") {
      fromStarter.mutate({ key: starter, name }, open);
      return;
    }
    create.mutate({ name, channel, content: channel === "email" ? starterEmail(account.brand_color) : { body: "" } }, open);
  };
  return (
    <Modal title="Create template" onClose={onClose} size={channel === "email" ? "lg" : "sm"}>
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
        {channel === "email" ? (
          <Field label="Start from" hint="Starter designs use your brand color, logo and social links.">
            <StarterGallery blank selected={starter} onPick={(s) => setStarter(s.key)} className="max-h-[50vh] overflow-y-auto p-0.5" />
          </Field>
        ) : null}
        <div className={cx("flex justify-end gap-2")}>
          <Button variant="ghost" onClick={onClose}>
            Cancel
          </Button>
          <Button type="submit" variant="primary" loading={create.isPending || fromStarter.isPending}>
            Create and edit
          </Button>
        </div>
      </form>
    </Modal>
  );
}
