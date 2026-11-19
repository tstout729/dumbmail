import { Link } from "react-router";
import { ExternalLink, FileText, Image as ImageIcon, Loader2, Reply } from "lucide-react";
import { useSession } from "../../api";
import { VariableMenu } from "../../components/email/VariableMenu";
import { Badge, Field, Input, Notice, Select, Skeleton, cx } from "../../components/ui";
import { useDebouncedValue } from "../../lib/hooks";
import { useWhatsappPreview, useWhatsappTemplates } from "./api";
import type { WhatsappContent, WhatsappPreview, WhatsappTemplate } from "./types";

type Props = { value: WhatsappContent; onChange: (v: WhatsappContent) => void; readOnly?: boolean };

export function categoryLabel(c: string): string {
  return c === "MARKETING" ? "Marketing" : c === "UTILITY" ? "Utility" : c === "AUTHENTICATION" ? "Authentication" : c;
}

export function statusTone(s: string): "green" | "amber" | "red" | "gray" {
  if (s === "APPROVED") return "green";
  if (s === "PENDING" || s === "IN_APPEAL") return "amber";
  if (s === "REJECTED" || s === "PAUSED" || s === "DISABLED") return "red";
  return "gray";
}

/** WhatsApp content for a campaign or flow step: pick an approved template and fill its variables, with a phone preview. */
export function WhatsappEditor({ value, onChange, readOnly }: Props) {
  const templates = useWhatsappTemplates();
  const debounced = useDebouncedValue(value, 400);
  const preview = useWhatsappPreview(debounced);
  const list = templates.data?.templates ?? [];
  const chosen = list.find((t) => t.id === value.template_id) ?? null;
  const set = (patch: Partial<WhatsappContent>) => onChange({ ...value, ...patch });

  return (
    <div className="grid grid-cols-1 gap-6 lg:grid-cols-[minmax(0,1fr)_320px]">
      <fieldset disabled={readOnly} className="min-w-0 space-y-4">
        <Field label="Template" hint="WhatsApp only lets a business start a conversation with a template Meta approved.">
          {templates.isPending ? (
            <Skeleton className="h-9 w-full" />
          ) : (
            <Select value={value.template_id ?? ""} onChange={(e) => onChange({ template_id: e.target.value || null, body: {} })} aria-label="Template">
              <option value="">Pick a template…</option>
              {list.map((t) => (
                <option key={t.id} value={t.id} disabled={!t.sendable}>
                  {t.name} ({t.language}) · {categoryLabel(t.category)}
                  {t.sendable ? "" : ` · ${t.status.toLowerCase()}`}
                </option>
              ))}
            </Select>
          )}
        </Field>
        {templates.data && list.length === 0 ? (
          <Notice tone="gray" title="No templates yet">
            Connect WhatsApp and sync or create templates in <Link to="/settings/whatsapp" className="font-medium underline">Settings → WhatsApp</Link>.
          </Notice>
        ) : null}
        {chosen ? <Variables template={chosen} value={value} set={set} /> : null}
        {chosen?.category === "MARKETING" ? (
          <p className="text-xs text-zinc-500">Marketing templates only go to people who opted in to WhatsApp messages from you.</p>
        ) : chosen ? (
          <p className="text-xs text-zinc-500">
            Utility templates go to people who opted in to WhatsApp, and, from flows marked as service messages, to anyone about their own order or booking.
          </p>
        ) : null}
      </fieldset>
      <WhatsappPhone preview={preview.data ?? null} loading={preview.isFetching} error={preview.error?.message ?? null} empty={!value.template_id} />
    </div>
  );
}

function Variables({ template, value, set }: { template: WhatsappTemplate; value: WhatsappContent; set: (p: Partial<WhatsappContent>) => void }) {
  const v = template.variables;
  const body = value.body ?? {};
  const buttons = value.buttons ?? {};
  const bodyText = template.components.find((c) => c.type.toUpperCase() === "BODY")?.text ?? "";
  return (
    <div className="space-y-3">
      <div className="rounded-md bg-zinc-50 px-3 py-2 text-xs whitespace-pre-wrap text-zinc-600">{bodyText}</div>
      {v.header ? (
        <VarInput label={`Header {{${v.header}}}`} value={value.header ?? ""} onChange={(s) => set({ header: s })} placeholder="{{ event.properties.order_id }}" />
      ) : null}
      {v.header_media ? (
        <Field label={`Header ${v.header_media.toLowerCase()}`} hint="A public https link to the file.">
          <Input value={value.header_media_url ?? ""} onChange={(e) => set({ header_media_url: e.target.value || undefined })} placeholder="https://…/banner.jpg" />
        </Field>
      ) : null}
      {v.body.map((name) => (
        <VarInput key={name} label={`{{${name}}}`} value={body[name] ?? ""} onChange={(s) => set({ body: { ...body, [name]: s } })} placeholder="{{ first_name|default('there') }}" />
      ))}
      {v.url_buttons.map((i) => (
        <VarInput
          key={i}
          label={`Button ${i + 1} link ending`}
          value={buttons[String(i)] ?? ""}
          onChange={(s) => set({ buttons: { ...buttons, [String(i)]: s } })}
          placeholder="{{ event.properties.order_id }}"
        />
      ))}
      {v.body.length === 0 && !v.header && !v.header_media && v.url_buttons.length === 0 ? <p className="text-xs text-zinc-500">This template has nothing to fill in.</p> : null}
    </div>
  );
}

function VarInput({ label, value, onChange, placeholder }: { label: string; value: string; onChange: (s: string) => void; placeholder: string }) {
  return (
    <Field label={label}>
      <div className="flex gap-2">
        <Input value={value} onChange={(e) => onChange(e.target.value)} placeholder={placeholder} aria-label={label} />
        <VariableMenu compact onInsert={(t) => onChange(value + t)} />
      </div>
    </Field>
  );
}

/** A phone-style view of the message as the sample person would get it. */
export function WhatsappPhone({ preview, loading, error, empty }: { preview: WhatsappPreview | null; loading?: boolean; error?: string | null; empty?: boolean }) {
  const { account } = useSession();
  return (
    <div className="space-y-2">
      <div className="flex items-center justify-between">
        <div className="text-xs font-medium text-zinc-700">Preview</div>
        {loading ? <Loader2 className="size-3.5 animate-spin text-zinc-400" /> : null}
      </div>
      <div className="mx-auto w-full max-w-[300px] rounded-[2.2rem] border-[6px] border-zinc-900 bg-zinc-900">
        <div className="flex items-center gap-2 rounded-t-[1.7rem] bg-emerald-800 px-4 pb-2 pt-5 text-white">
          <span className="flex size-7 items-center justify-center rounded-full bg-white/20 text-xs font-semibold">{account.name.slice(0, 1).toUpperCase()}</span>
          <span className="truncate text-sm font-medium">{account.name}</span>
        </div>
        <div className="min-h-[260px] rounded-b-[1.7rem] bg-[#efeae2] p-3">
          {empty ? (
            <p className="pt-16 text-center text-xs text-zinc-500">Pick a template to see it here.</p>
          ) : !preview ? (
            <Skeleton className="h-24 w-4/5" />
          ) : (
            <div className="w-[88%] space-y-1">
              <div className="rounded-lg rounded-tl-none bg-white p-2 shadow-sm">
                {preview.header_media ? (
                  <div className="mb-2 flex h-28 items-center justify-center overflow-hidden rounded bg-zinc-100 text-zinc-400">
                    {preview.header_media === "IMAGE" && preview.header_media_url ? (
                      <img src={preview.header_media_url} alt="" className="h-full w-full object-cover" />
                    ) : preview.header_media === "IMAGE" ? (
                      <ImageIcon className="size-6" />
                    ) : (
                      <FileText className="size-6" />
                    )}
                  </div>
                ) : null}
                <p className="whitespace-pre-wrap break-words text-[13px] leading-snug text-zinc-900">{preview.text}</p>
              </div>
              {(preview.buttons ?? []).map((b, i) => (
                <div key={i} className="flex items-center justify-center gap-1 rounded-lg bg-white py-1.5 text-[13px] font-medium text-sky-600 shadow-sm" title={b.url ?? undefined}>
                  {b.type === "URL" ? <ExternalLink className="size-3" /> : <Reply className="size-3" />}
                  {b.text}
                </div>
              ))}
            </div>
          )}
        </div>
      </div>
      {preview?.template ? (
        <div className="flex flex-wrap justify-center gap-1.5">
          <Badge tone={statusTone(preview.template.status)}>{preview.template.status.toLowerCase()}</Badge>
          <Badge>{categoryLabel(preview.template.category)}</Badge>
        </div>
      ) : null}
      {error ? <p className="text-xs text-red-600">{error}</p> : null}
      {preview && preview.problems.length > 0 ? (
        <ul className={cx("space-y-0.5 text-xs text-amber-700")}>
          {preview.problems.map((p) => (
            <li key={p}>• {p}</li>
          ))}
        </ul>
      ) : null}
    </div>
  );
}
