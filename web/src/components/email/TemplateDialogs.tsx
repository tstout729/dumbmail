import { useState, type FormEvent } from "react";
import { LayoutTemplate } from "lucide-react";
import type { Channel, EmailContent, SmsContent, Template } from "../../api";
import { useCreateTemplate, useTemplates } from "../../api";
import { toast } from "../../lib/toast";
import { formatRelative } from "../../lib/format";
import { Button, EmptyState, Field, Input, Modal, Skeleton, Tabs, cx } from "../ui";
import { StarterGallery, starterAsTemplate } from "../../features/content/StarterGallery";

export function LoadTemplateDialog({ channel, onPick, onClose }: { channel: Channel; onPick: (t: Template) => void; onClose: () => void }) {
  const templates = useTemplates(channel);
  const [tab, setTab] = useState<"mine" | "starters">("mine");
  const [loading, setLoading] = useState(false);
  return (
    <Modal title="Load from template" description="This replaces the current content." onClose={onClose} size="lg">
      {channel === "email" ? (
        <Tabs
          value={tab}
          onChange={setTab}
          tabs={[
            { value: "mine", label: "Your templates" },
            { value: "starters", label: "Starter designs" },
          ]}
        />
      ) : null}
      {tab === "starters" ? (
        <div className={cx("max-h-[60vh] overflow-y-auto", loading && "pointer-events-none opacity-60")}>
          <StarterGallery
            onPick={(s) => {
              setLoading(true);
              starterAsTemplate(s.key)
                .then((t) => {
                  onPick(t);
                  onClose();
                })
                .catch((e: unknown) => toast.error(e instanceof Error ? e.message : "Couldn't load that design"))
                .finally(() => setLoading(false));
            }}
          />
        </div>
      ) : templates.isPending ? (
        <div className="space-y-2">
          {[0, 1, 2].map((i) => (
            <Skeleton key={i} className="h-12 w-full" />
          ))}
        </div>
      ) : (templates.data ?? []).length === 0 ? (
        <EmptyState icon={<LayoutTemplate className="size-5" />} title="No templates yet" description="Save any message as a template to reuse it here, or start from a starter design." />
      ) : (
        <ul className="max-h-[60vh] divide-y divide-zinc-100 overflow-y-auto rounded-lg border border-zinc-200">
          {(templates.data ?? []).map((t) => (
            <li key={t.id}>
              <button
                type="button"
                onClick={() => {
                  onPick(t);
                  onClose();
                }}
                className="flex w-full items-center justify-between gap-3 px-4 py-3 text-left hover:bg-zinc-50"
              >
                <span>
                  <span className="block text-sm font-medium text-zinc-900">{t.name}</span>
                  <span className="block truncate text-xs text-zinc-500">
                    {"subject" in t.content ? t.content.subject || "No subject" : t.content.body.slice(0, 80)}
                  </span>
                </span>
                <span className="shrink-0 text-xs text-zinc-400">Updated {formatRelative(t.updated_at)}</span>
              </button>
            </li>
          ))}
        </ul>
      )}
    </Modal>
  );
}

type SaveProps = { channel: Channel; content: EmailContent | SmsContent; defaultName?: string; onClose: () => void };

export function SaveTemplateDialog({ channel, content, defaultName, onClose }: SaveProps) {
  const create = useCreateTemplate();
  const onSubmit = (e: FormEvent<HTMLFormElement>) => {
    e.preventDefault();
    const name = String(new FormData(e.currentTarget).get("name")).trim();
    create.mutate(
      { name, channel, content },
      {
        onSuccess: () => {
          toast.success("Saved as template");
          onClose();
        },
      },
    );
  };
  return (
    <Modal title="Save as template" onClose={onClose} size="sm">
      <form onSubmit={onSubmit} className="space-y-4">
        <Field label="Template name">
          <Input name="name" required autoFocus defaultValue={defaultName} />
        </Field>
        <div className="flex justify-end gap-2">
          <Button variant="ghost" onClick={onClose}>
            Cancel
          </Button>
          <Button type="submit" variant="primary" loading={create.isPending}>
            Save template
          </Button>
        </div>
      </form>
    </Modal>
  );
}
