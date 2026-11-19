import { useState, type ReactNode } from "react";
import { ExternalLink, Plus, Trash2 } from "lucide-react";
import { Link } from "react-router";
import { Button, Field, IconButton, Input, Segmented, Select } from "../../components/ui";
import { rcsPreviewOf, useRcsSettings } from "./api";
import type { RcsRich, RcsSuggestion, SmsWithRich } from "./types";

const MAX_SUGGESTIONS = 4;
const MAX_LABEL = 25;

function clean(rich: RcsRich): RcsRich | undefined {
  const out: RcsRich = {};
  if (rich.card) out.card = rich.card;
  if (rich.suggestions && rich.suggestions.length > 0) out.suggestions = rich.suggestions;
  return out.card || out.suggestions ? out : undefined;
}

type EditorProps = { value: SmsWithRich; onChange: (v: SmsWithRich) => void; readOnly?: boolean };

/** The optional rich version of a text, for phones that get RCS: a card and up to four suggestions. */
export function RichVersionEditor({ value, onChange, readOnly }: EditorProps) {
  const settings = useRcsSettings();
  const rich = value.rcs ?? {};
  const [open, setOpen] = useState(Boolean(value.rcs));
  const available = settings.data?.available ?? false;
  // On a server without RCS, keep the editor out of the way unless the text already has one.
  if (!available && !value.rcs) return null;
  const set = (next: RcsRich) => onChange({ ...value, rcs: clean(next) });
  const suggestions = rich.suggestions ?? [];
  const setSuggestion = (i: number, s: RcsSuggestion) => set({ ...rich, suggestions: suggestions.map((x, j) => (j === i ? s : x)) });

  if (!open) {
    return (
      <div className="rounded-lg border border-dashed border-zinc-300 px-3 py-2.5">
        <div className="flex flex-wrap items-center justify-between gap-2">
          <div className="min-w-0">
            <div className="text-xs font-medium text-zinc-700">Rich version (RCS)</div>
            <p className="text-xs text-zinc-500">Add a picture card and buttons for phones that get RCS. Everyone else gets the text above.</p>
          </div>
          {readOnly ? null : (
            <Button size="sm" variant="ghost" icon={<Plus className="size-3.5" />} onClick={() => setOpen(true)}>
              Add rich version
            </Button>
          )}
        </div>
      </div>
    );
  }

  return (
    <div className="space-y-3 rounded-lg border border-zinc-200 px-3 py-3">
      <div className="flex items-start justify-between gap-2">
        <div className="min-w-0 flex-1">
          <div className="text-xs font-medium text-zinc-700">Rich version (RCS)</div>
          <p className="text-xs text-zinc-500">
            Phones that get RCS see this from your business&apos;s name, with the text above as the message.{" "}
            {settings.data && !settings.data.sending ? (
              <>
                RCS is off for this account; turn it on in{" "}
                <Link to="/settings/sms" className="text-accent-700 underline">
                  Settings → SMS
                </Link>
                .
              </>
            ) : null}
          </p>
        </div>
        {readOnly ? null : (
          <Button
            size="sm"
            variant="ghost"
            icon={<Trash2 className="size-3.5" />}
            onClick={() => {
              onChange({ ...value, rcs: undefined });
              setOpen(false);
            }}
          >
            Remove
          </Button>
        )}
      </div>
      <fieldset disabled={readOnly} className="min-w-0 space-y-3">
        {rich.card ? (
          <div className="grid grid-cols-1 gap-3 sm:grid-cols-2">
            <Field label="Card image" hint="An https link to a JPEG, PNG or GIF.">
              <Input value={rich.card.image_url} onChange={(e) => set({ ...rich, card: { image_url: e.target.value, title: rich.card?.title ?? "" } })} placeholder="https://…/image.jpg" />
            </Field>
            <Field label="Card title" hint="Your text shows under it. Variables work here too.">
              <Input value={rich.card.title} maxLength={200} onChange={(e) => set({ ...rich, card: { image_url: rich.card?.image_url ?? "", title: e.target.value } })} placeholder="Spring sale" />
            </Field>
            <div className="sm:col-span-2">
              <Button size="sm" variant="ghost" icon={<Trash2 className="size-3.5" />} onClick={() => set({ ...rich, card: undefined })}>
                Remove card
              </Button>
            </div>
          </div>
        ) : (
          <Button size="sm" variant="secondary" icon={<Plus className="size-3.5" />} onClick={() => set({ ...rich, card: { image_url: "", title: "" } })}>
            Add a card
          </Button>
        )}
        <div className="space-y-2">
          <div className="text-xs font-medium text-zinc-700">Suggestions</div>
          {suggestions.length === 0 ? <p className="text-xs text-zinc-500">Buttons under the message: a reply they can tap, or a link.</p> : null}
          {suggestions.map((s, i) => (
            <div key={i} className="flex flex-wrap items-center gap-2">
              <Select
                aria-label="Suggestion type"
                className="w-28 shrink-0"
                value={s.type}
                onChange={(e) => setSuggestion(i, e.target.value === "url" ? { type: "url", text: s.text, url: s.url ?? "" } : { type: "reply", text: s.text })}
              >
                <option value="reply">Reply</option>
                <option value="url">Link</option>
              </Select>
              <Input
                aria-label="Button label"
                className="w-40 min-w-0 flex-1 sm:flex-none"
                value={s.text}
                maxLength={MAX_LABEL}
                placeholder={s.type === "url" ? "Shop now" : "Yes please"}
                onChange={(e) => setSuggestion(i, { ...s, text: e.target.value })}
              />
              <IconButton label="Remove suggestion" className="sm:order-last" onClick={() => set({ ...rich, suggestions: suggestions.filter((_, j) => j !== i) })}>
                <Trash2 className="size-4" />
              </IconButton>
              {s.type === "url" ? (
                <Input aria-label="Link" className="w-full min-w-0 sm:w-auto sm:flex-1" value={s.url ?? ""} placeholder="https://…" onChange={(e) => setSuggestion(i, { ...s, url: e.target.value })} />
              ) : (
                <p className="w-full text-xs text-zinc-500 sm:w-auto sm:flex-1">A tap sends this back as their reply.</p>
              )}
            </div>
          ))}
          {suggestions.length < MAX_SUGGESTIONS ? (
            <Button size="sm" variant="secondary" icon={<Plus className="size-3.5" />} onClick={() => set({ ...rich, suggestions: [...suggestions, { type: "reply", text: "" }] })}>
              Add a suggestion
            </Button>
          ) : null}
        </div>
      </fieldset>
    </div>
  );
}

type PreviewProps = { value: SmsWithRich; render: object | undefined; body: string; from: string; sms: ReactNode };

/** The phone preview both ways when a text has a rich version: as RCS, and as the SMS other phones get. */
export function RcsPreviewSwitch({ value, render, body, from, sms }: PreviewProps) {
  const [view, setView] = useState<"rcs" | "sms">("rcs");
  if (!value.rcs) return <>{sms}</>;
  const shown = rcsPreviewOf(render);
  // The server render fills the title in; until it arrives, show what was typed.
  const card = shown ? shown.card : value.rcs.card ? { image_url: value.rcs.card.image_url || null, title: value.rcs.card.title || null } : null;
  const suggestions = shown ? shown.suggestions : (value.rcs.suggestions ?? []);
  return (
    <div className="space-y-2">
      <div className="flex justify-center">
        <Segmented
          size="sm"
          value={view}
          onChange={setView}
          options={[
            { value: "rcs", label: "RCS phones" },
            { value: "sms", label: "Other phones" },
          ]}
        />
      </div>
      {view === "sms" ? sms : <RcsPhone body={body} from={from} card={card} suggestions={suggestions} />}
    </div>
  );
}

function RcsPhone({ body, from, card, suggestions }: { body: string; from: string; card: { image_url: string | null; title: string | null } | null; suggestions: RcsSuggestion[] }) {
  const chips = suggestions.filter((s) => s.text.trim());
  const buttons = (
    <div className="flex flex-wrap gap-1.5">
      {chips.map((s, i) => (
        <span key={i} className="inline-flex items-center gap-1 rounded-full border border-accent-300 px-2.5 py-1 text-[12px] font-medium text-accent-800">
          {s.type === "url" ? <ExternalLink className="size-3" /> : null}
          {s.text}
        </span>
      ))}
    </div>
  );
  return (
    <div className="mx-auto w-full max-w-[300px]">
      <div className="rounded-[2.2rem] border-[6px] border-zinc-900 bg-white shadow-lg">
        <div className="flex flex-col items-center border-b border-zinc-100 pb-2 pt-4">
          <div className="flex size-9 items-center justify-center rounded-full bg-accent-100 text-xs font-semibold text-accent-800">{from.slice(0, 1).toUpperCase()}</div>
          <div className="mt-1 flex items-center gap-1 text-[11px] text-zinc-600">
            {from}
            <span className="rounded bg-zinc-100 px-1 text-[9px] font-semibold uppercase tracking-wide text-zinc-500">RCS</span>
          </div>
        </div>
        <div className="min-h-[300px] space-y-2 px-3 py-4">
          {card ? (
            <div className="max-w-[90%] overflow-hidden rounded-2xl border border-zinc-200 bg-white">
              {card.image_url ? <img src={card.image_url} alt="" className="h-32 w-full bg-zinc-100 object-cover" /> : null}
              <div className="space-y-1 px-3 py-2">
                {card.title ? <div className="text-[13px] font-semibold text-zinc-900">{card.title}</div> : null}
                <div className="whitespace-pre-wrap break-words text-[12px] leading-snug text-zinc-700">{body || "Your message"}</div>
              </div>
              {chips.length > 0 ? <div className="border-t border-zinc-100 px-3 py-2">{buttons}</div> : null}
            </div>
          ) : (
            <>
              <div className="max-w-[85%] whitespace-pre-wrap break-words rounded-2xl rounded-bl-md bg-zinc-200/80 px-3 py-2 text-[13px] leading-snug text-zinc-900">{body || "Your message"}</div>
              {chips.length > 0 ? buttons : null}
            </>
          )}
        </div>
      </div>
    </div>
  );
}
