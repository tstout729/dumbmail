import { useRef, useState } from "react";
import { ChevronDown, LayoutTemplate, Save } from "lucide-react";
import type { EmailContent } from "../../api";
import { useSession } from "../../api";
import { Button, ColorInput, Field, Input, Segmented, Textarea, cx } from "../ui";
import { BlockList } from "./BlockList";
import { EmailPreview } from "./EmailPreview";
import { LoadTemplateDialog, SaveTemplateDialog } from "./TemplateDialogs";
import { VariableMenu, insertAtCursor } from "./VariableMenu";
import { newBlock } from "./blocks";

type Props = {
  value: EmailContent;
  onChange: (value: EmailContent) => void;
  /** Show subject/preheader/sender fields (templates may hide sender). */
  showSender?: boolean;
  /** Hide the template load/save actions (e.g. when editing a template itself). */
  templates?: boolean;
  templateName?: string;
  readOnly?: boolean;
  className?: string;
};

/** Shared editor for campaign emails, flow email nodes and templates. */
export function EmailEditor({ value, onChange, showSender = true, templates = true, templateName, readOnly, className }: Props) {
  const { account } = useSession();
  const subjectRef = useRef<HTMLInputElement>(null);
  const [showMore, setShowMore] = useState(false);
  const [dialog, setDialog] = useState<"load" | "save" | null>(null);
  const mode: "blocks" | "html" = value.blocks === undefined && value.html !== undefined ? "html" : "blocks";
  const set = (patch: Partial<EmailContent>) => onChange({ ...value, ...patch });

  const setMode = (next: "blocks" | "html") => {
    if (next === mode) return;
    if (next === "html") set({ blocks: undefined, html: value.html ?? "<p>Write your HTML here.</p>" });
    else set({ blocks: [newBlock("heading"), newBlock("text")], html: undefined });
  };

  return (
    <div className={cx("flex min-h-0 flex-col gap-4", className)}>
      <fieldset disabled={readOnly} className="min-w-0 grid gap-3 md:grid-cols-2">
        <Field label="Subject line" className="md:col-span-1">
          <div className="flex items-center gap-1">
            <Input
              ref={subjectRef}
              value={value.subject}
              onChange={(e) => set({ subject: e.target.value })}
              placeholder="What's in it for them?"
            />
            <VariableMenu compact onInsert={(v) => set({ subject: insertAtCursor(subjectRef.current, value.subject, v) })} />
          </div>
        </Field>
        <Field label="Preview text" hint="Shown after the subject in most inboxes.">
          <Input value={value.preheader ?? ""} onChange={(e) => set({ preheader: e.target.value || undefined })} />
        </Field>
        {showSender ? (
          <>
            <Field label="From name">
              <Input value={value.from_name ?? ""} onChange={(e) => set({ from_name: e.target.value || undefined })} placeholder={account.from_name ?? account.name} />
            </Field>
            <Field label="From email">
              <Input type="email" value={value.from_email ?? ""} onChange={(e) => set({ from_email: e.target.value || undefined })} placeholder={account.from_email ?? "hello@yourdomain.com"} />
            </Field>
          </>
        ) : null}
        <div className="md:col-span-2">
          <button type="button" onClick={() => setShowMore(!showMore)} className="inline-flex items-center gap-1 text-xs font-medium text-zinc-600 hover:text-zinc-900">
            <ChevronDown className={cx("size-3.5 transition-transform", showMore && "rotate-180")} />
            Reply-to and theme
          </button>
          {showMore ? (
            <div className="mt-3 grid gap-3 md:grid-cols-3">
              <Field label="Reply-to">
                <Input type="email" value={value.reply_to ?? ""} onChange={(e) => set({ reply_to: e.target.value || undefined })} placeholder={account.reply_to ?? ""} />
              </Field>
              <Field label="Brand color">
                <ColorInput value={value.theme?.brand_color ?? account.brand_color} onChange={(brand_color) => set({ theme: { ...value.theme, brand_color } })} />
              </Field>
              <Field label="Background">
                <ColorInput value={value.theme?.background ?? "#f4f4f5"} onChange={(background) => set({ theme: { ...value.theme, background } })} />
              </Field>
            </div>
          ) : null}
        </div>
      </fieldset>

      <div className="grid min-h-0 flex-1 gap-4 lg:grid-cols-[minmax(340px,440px)_1fr]">
        <div className="flex min-h-0 flex-col">
          <div className="flex flex-wrap items-center justify-between gap-2 pb-2">
            <Segmented
              size="sm"
              value={mode}
              onChange={setMode}
              options={[
                { value: "blocks", label: "Blocks" },
                { value: "html", label: "Custom HTML" },
              ]}
            />
            {templates && !readOnly ? (
              <div className="flex gap-1">
                <Button size="sm" variant="ghost" icon={<LayoutTemplate className="size-3.5" />} onClick={() => setDialog("load")}>
                  Load template
                </Button>
                <Button size="sm" variant="ghost" icon={<Save className="size-3.5" />} onClick={() => setDialog("save")}>
                  Save as template
                </Button>
              </div>
            ) : null}
          </div>
          <fieldset disabled={readOnly} className="min-w-0 min-h-0 flex-1 overflow-y-auto pr-1">
            {mode === "blocks" ? (
              <BlockList blocks={value.blocks ?? []} onChange={(blocks) => set({ blocks })} />
            ) : (
              <Textarea
                value={value.html ?? ""}
                onChange={(e) => set({ html: e.target.value })}
                rows={24}
                className="font-mono text-xs"
                aria-label="Email HTML"
              />
            )}
          </fieldset>
        </div>
        <EmailPreview content={value} fromName={account.from_name ?? account.name} className="min-h-[480px]" />
      </div>

      {dialog === "load" ? (
        <LoadTemplateDialog
          channel="email"
          onPick={(t) => {
            if ("subject" in t.content) {
              const c = t.content;
              onChange({ ...value, ...c, subject: c.subject || value.subject, from_name: c.from_name ?? value.from_name, from_email: c.from_email ?? value.from_email });
            }
          }}
          onClose={() => setDialog(null)}
        />
      ) : null}
      {dialog === "save" ? <SaveTemplateDialog channel="email" content={value} defaultName={templateName ?? value.subject} onClose={() => setDialog(null)} /> : null}
    </div>
  );
}
