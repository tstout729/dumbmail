import { useRef, useState } from "react";
import { LayoutTemplate, Loader2, Save } from "lucide-react";
import type { SmsContent } from "../../api";
import { useSession, useSmsRender } from "../../api";
import { useDebouncedValue } from "../../lib/hooks";
import { ShortLinkNote } from "../../features/shortLinks/ShortLinkNote";
import { Button, Field, Input, Textarea, cx } from "../ui";
import { LoadTemplateDialog, SaveTemplateDialog } from "./TemplateDialogs";
import { VariableMenu, insertAtCursor } from "./VariableMenu";

const gsm7 = /^[\n\r @£$¥èéùìòÇØøÅåΔ_ΦΓΛΩΠΨΣΘΞÆæßÉ!"#¤%&'()*+,\-./0-9:;<=>?¡A-ZÄÖÑÜ§¿a-zäöñüà^{}\\[~\]|€]*$/;

/** Local estimate; the server render (`/api/render`) is authoritative once it returns. */
export function smsSegments(body: string): { characters: number; segments: number; unicode: boolean } {
  const unicode = !gsm7.test(body);
  const characters = [...body].length;
  const single = unicode ? 70 : 160;
  const multi = unicode ? 67 : 153;
  const segments = characters === 0 ? 0 : characters <= single ? 1 : Math.ceil(characters / multi);
  return { characters, segments, unicode };
}

type Props = { value: SmsContent; onChange: (v: SmsContent) => void; templates?: boolean; templateName?: string; readOnly?: boolean };

export function SmsEditor({ value, onChange, templates = true, templateName, readOnly }: Props) {
  const { account } = useSession();
  const ref = useRef<HTMLTextAreaElement>(null);
  const [dialog, setDialog] = useState<"load" | "save" | null>(null);
  const debounced = useDebouncedValue(value, 400);
  const render = useSmsRender(debounced.body ? debounced : null);
  const local = smsSegments(value.body);
  const settled = debounced === value && render.data;
  const characters = settled ? render.data.characters : local.characters;
  const segments = settled ? render.data.segments : local.segments;

  return (
    <div className="grid grid-cols-1 gap-6 lg:grid-cols-[minmax(0,1fr)_320px]">
      <fieldset disabled={readOnly} className="min-w-0 space-y-3">
        <div className="flex flex-wrap items-center justify-between gap-2">
          <div className="text-xs font-medium text-zinc-700">Message</div>
          <div className="flex items-center gap-1">
            <VariableMenu onInsert={(v) => onChange({ ...value, body: insertAtCursor(ref.current, value.body, v) })} />
            {templates && !readOnly ? (
              <>
                <Button size="sm" variant="ghost" icon={<LayoutTemplate className="size-3.5" />} onClick={() => setDialog("load")}>
                  Load
                </Button>
                <Button size="sm" variant="ghost" icon={<Save className="size-3.5" />} onClick={() => setDialog("save")}>
                  Save as template
                </Button>
              </>
            ) : null}
          </div>
        </div>
        <Textarea
          ref={ref}
          rows={7}
          value={value.body}
          onChange={(e) => onChange({ ...value, body: e.target.value })}
          placeholder={`${account.name}: Hi {{ first_name|default("there") }}! … Reply STOP to opt out`}
          aria-label="SMS body"
        />
        <div className="flex flex-wrap items-center gap-3 text-xs text-zinc-500">
          <span className="tabular-nums">{characters} characters</span>
          <span className={cx("tabular-nums", segments > 2 && "font-medium text-amber-700")}>
            {segments} segment{segments === 1 ? "" : "s"}
          </span>
          {local.unicode ? <span className="text-amber-700">Contains emoji or special characters (70 per segment)</span> : null}
          <ShortLinkNote body={value.body} shortened={settled ? settled.short_links : undefined} />
          {render.isFetching ? <Loader2 className="size-3 animate-spin" /> : null}
        </div>
        <Field label="Image URL (MMS, optional)">
          <Input value={value.media_url ?? ""} onChange={(e) => onChange({ ...value, media_url: e.target.value || undefined })} placeholder="https://…/image.jpg" />
        </Field>
        <p className="text-xs text-zinc-500">Include your brand name and opt-out language. Carriers require it.</p>
      </fieldset>
      <PhonePreview body={render.data?.body ?? value.body} mediaUrl={value.media_url} from={account.sms_from ?? account.name} />
      {dialog === "load" ? (
        <LoadTemplateDialog
          channel="sms"
          onPick={(t) => {
            if ("body" in t.content) onChange(t.content);
          }}
          onClose={() => setDialog(null)}
        />
      ) : null}
      {dialog === "save" ? <SaveTemplateDialog channel="sms" content={value} defaultName={templateName} onClose={() => setDialog(null)} /> : null}
    </div>
  );
}

export function PhonePreview({ body, mediaUrl, from }: { body: string; mediaUrl?: string; from: string }) {
  return (
    <div className="mx-auto w-full max-w-[300px]">
      <div className="rounded-[2.2rem] border-[6px] border-zinc-900 bg-white shadow-lg">
        <div className="flex flex-col items-center border-b border-zinc-100 pb-2 pt-4">
          <div className="flex size-9 items-center justify-center rounded-full bg-zinc-200 text-xs font-semibold text-zinc-600">
            {from.slice(0, 1).toUpperCase()}
          </div>
          <div className="mt-1 text-[11px] text-zinc-600">{from}</div>
        </div>
        <div className="min-h-[300px] space-y-1.5 px-3 py-4">
          {mediaUrl ? <img src={mediaUrl} alt="" className="max-w-[80%] rounded-2xl" /> : null}
          {body ? (
            <div className="max-w-[85%] whitespace-pre-wrap break-words rounded-2xl rounded-bl-md bg-zinc-200/80 px-3 py-2 text-[13px] leading-snug text-zinc-900">
              {body}
            </div>
          ) : (
            <div className="text-center text-xs text-zinc-400">Your message preview</div>
          )}
        </div>
      </div>
    </div>
  );
}
