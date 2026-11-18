import { AlignCenter, AlignLeft, AlignRight, Plus, Sparkles, Trash2 } from "lucide-react";
import type { Align, EmailBlock } from "../../api/types";
import { Button, Checkbox, ColorInput, Field, IconButton, Input, Segmented, Select, Textarea } from "../../components/ui";
import { ImageInput } from "../../pages/forms/ImageInput";
import { useSession } from "../../api";
import { toast } from "../../lib/toast";
import { useVideoThumbnail } from "./api";
import type { ContentBlock, IconStyle, NavLink, SocialLink, SocialNetwork, TableColumn } from "./types";

type Of<T extends ContentBlock["type"]> = Extract<ContentBlock, { type: T }>;
type Props<T extends ContentBlock["type"]> = { block: Of<T>; onChange: (b: EmailBlock) => void };

const networks: { value: SocialNetwork; label: string }[] = [
  { value: "instagram", label: "Instagram" },
  { value: "facebook", label: "Facebook" },
  { value: "tiktok", label: "TikTok" },
  { value: "x", label: "X" },
  { value: "youtube", label: "YouTube" },
  { value: "pinterest", label: "Pinterest" },
  { value: "linkedin", label: "LinkedIn" },
  { value: "threads", label: "Threads" },
  { value: "website", label: "Website" },
  { value: "email", label: "Email" },
];

const iconStyles: { value: IconStyle; label: string }[] = [
  { value: "color", label: "Color" },
  { value: "dark", label: "Dark" },
  { value: "light", label: "Light" },
  { value: "white", label: "White" },
];

function AlignPicker({ value, onChange }: { value: Align | undefined; onChange: (a: Align) => void }) {
  return (
    <Segmented
      size="sm"
      value={value ?? "left"}
      onChange={onChange}
      options={[
        { value: "left", label: <AlignLeft className="size-3.5" aria-label="Left" /> },
        { value: "center", label: <AlignCenter className="size-3.5" aria-label="Center" /> },
        { value: "right", label: <AlignRight className="size-3.5" aria-label="Right" /> },
      ]}
    />
  );
}

function LinkRows({ links, onChange, max = 8 }: { links: NavLink[]; onChange: (l: NavLink[]) => void; max?: number }) {
  return (
    <div className="space-y-1.5">
      {links.map((l, i) => (
        <div key={i} className="flex items-center gap-1.5">
          <Input value={l.text} onChange={(e) => onChange(links.map((x, j) => (j === i ? { ...x, text: e.target.value } : x)))} placeholder="Label" aria-label={`Link ${i + 1} label`} className="w-28 shrink-0" />
          <Input value={l.href} onChange={(e) => onChange(links.map((x, j) => (j === i ? { ...x, href: e.target.value } : x)))} placeholder="https://" aria-label={`Link ${i + 1} address`} />
          <IconButton size="sm" label="Remove link" onClick={() => onChange(links.filter((_, j) => j !== i))}>
            <Trash2 className="size-3" />
          </IconButton>
        </div>
      ))}
      {links.length < max ? (
        <Button size="sm" variant="ghost" icon={<Plus className="size-3.5" />} onClick={() => onChange([...links, { text: "", href: "https://" }])}>
          Add link
        </Button>
      ) : null}
    </div>
  );
}

function SocialRows({ links, onChange }: { links: SocialLink[]; onChange: (l: SocialLink[]) => void }) {
  const { account } = useSession();
  const accountLinks = Object.entries(account.social_links ?? {}).filter(([, v]) => typeof v === "string" && v);
  return (
    <div className="space-y-1.5">
      {links.length === 0 ? (
        <p className="text-xs text-zinc-500">
          {accountLinks.length > 0 ? `Shows your ${accountLinks.length} social links from Settings → Brand.` : "Add your social links in Settings → Brand, or add them here."}
        </p>
      ) : null}
      {links.map((l, i) => (
        <div key={i} className="flex items-center gap-1.5">
          <Select value={l.network} onChange={(e) => onChange(links.map((x, j) => (j === i ? { ...x, network: e.target.value as SocialNetwork } : x)))} className="w-32 shrink-0" aria-label="Network">
            {networks.map((n) => (
              <option key={n.value} value={n.value}>
                {n.label}
              </option>
            ))}
          </Select>
          <Input value={l.url} onChange={(e) => onChange(links.map((x, j) => (j === i ? { ...x, url: e.target.value } : x)))} placeholder={l.network === "email" ? "hello@yourbrand.com" : "https://"} aria-label={`${l.network} address`} />
          <IconButton size="sm" label="Remove" onClick={() => onChange(links.filter((_, j) => j !== i))}>
            <Trash2 className="size-3" />
          </IconButton>
        </div>
      ))}
      {links.length < 10 ? (
        <Button size="sm" variant="ghost" icon={<Plus className="size-3.5" />} onClick={() => onChange([...links, { network: "instagram", url: "https://" }])}>
          {links.length === 0 ? "Choose links for this email" : "Add network"}
        </Button>
      ) : null}
    </div>
  );
}

function SocialFields({ block, onChange }: Props<"social">) {
  return (
    <div className="space-y-3">
      <SocialRows links={block.links ?? []} onChange={(links) => onChange({ ...block, links: links.length ? links : undefined })} />
      <div className="flex flex-wrap items-center gap-2">
        <Segmented size="sm" value={block.style ?? "color"} onChange={(style) => onChange({ ...block, style })} options={iconStyles} />
        <Segmented
          size="sm"
          value={String(block.size ?? 32) as "24" | "32" | "40"}
          onChange={(v) => onChange({ ...block, size: Number(v) as 24 | 32 | 40 })}
          options={[
            { value: "24", label: "S" },
            { value: "32", label: "M" },
            { value: "40", label: "L" },
          ]}
        />
        <AlignPicker value={block.align} onChange={(align) => onChange({ ...block, align })} />
      </div>
    </div>
  );
}

function VideoFields({ block, onChange }: Props<"video">) {
  const thumb = useVideoThumbnail();
  const make = (input: { video_url?: string; image_url?: string }) =>
    thumb.mutate(input, {
      onSuccess: (r) => {
        onChange({ ...block, thumbnail: r.thumbnail_url });
        toast.success("Thumbnail ready");
      },
    });
  return (
    <div className="space-y-3">
      <Field label="Video link" hint="Where the play button goes: YouTube, Vimeo, or any page with the video.">
        <Input value={block.url} onChange={(e) => onChange({ ...block, url: e.target.value })} placeholder="https://www.youtube.com/watch?v=…" />
      </Field>
      <Field label="Thumbnail" hint="Email apps can't play video, so people see this picture with a play button.">
        <div className="space-y-2">
          <ImageInput value={block.thumbnail ?? ""} onChange={(thumbnail) => onChange({ ...block, thumbnail: thumbnail || undefined })} aria-label="Thumbnail image" />
          <div className="flex flex-wrap gap-1.5">
            <Button size="sm" variant="secondary" icon={<Sparkles className="size-3.5" />} loading={thumb.isPending} onClick={() => make({ video_url: block.url })}>
              Get from YouTube or Vimeo
            </Button>
            {block.thumbnail ? (
              <Button size="sm" variant="ghost" loading={thumb.isPending} onClick={() => make({ image_url: block.thumbnail })}>
                Add a play button to this image
              </Button>
            ) : null}
          </div>
        </div>
      </Field>
      <div className="grid grid-cols-1 gap-3 sm:grid-cols-2">
        <Field label="Alt text">
          <Input value={block.alt ?? ""} onChange={(e) => onChange({ ...block, alt: e.target.value || undefined })} />
        </Field>
        <Field label="Caption (optional)">
          <Input value={block.caption ?? ""} onChange={(e) => onChange({ ...block, caption: e.target.value || undefined })} />
        </Field>
      </div>
    </div>
  );
}

/** `datetime-local` value in the browser's time zone, from an ISO time. */
function toLocalInput(iso: string): string {
  const d = new Date(iso);
  if (Number.isNaN(d.getTime())) return "";
  const pad = (n: number) => String(n).padStart(2, "0");
  return `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())}T${pad(d.getHours())}:${pad(d.getMinutes())}`;
}

function CountdownFields({ block, onChange }: Props<"countdown">) {
  return (
    <div className="space-y-3">
      <Field label="Ends at" hint="In your browser's time zone. The timer shows the time left whenever the email is opened.">
        <Input
          type="datetime-local"
          value={toLocalInput(block.ends_at)}
          onChange={(e) => {
            const d = new Date(e.target.value);
            if (!Number.isNaN(d.getTime())) onChange({ ...block, ends_at: d.toISOString() });
          }}
        />
      </Field>
      <div className="grid grid-cols-1 gap-3 sm:grid-cols-3">
        <Field label="Background">
          <ColorInput value={block.background ?? "#111827"} onChange={(background) => onChange({ ...block, background })} />
        </Field>
        <Field label="Numbers">
          <ColorInput value={block.color ?? "#ffffff"} onChange={(color) => onChange({ ...block, color })} />
        </Field>
        <Field label="Labels">
          <ColorInput value={block.label_color ?? "#9ca3af"} onChange={(label_color) => onChange({ ...block, label_color })} />
        </Field>
      </div>
      <Field label="After it ends, show" hint="Letters, numbers and basic punctuation, up to 40 characters.">
        <Input value={block.ended_text ?? ""} maxLength={40} onChange={(e) => onChange({ ...block, ended_text: e.target.value })} placeholder="This offer has ended" />
      </Field>
    </div>
  );
}

function TableFields({ block, onChange }: Props<"table">) {
  const cols = block.columns;
  const rows = block.rows ?? [];
  const setCols = (columns: TableColumn[]) => {
    const n = columns.length;
    const fit = (r: string[]) => Array.from({ length: n }, (_, i) => r[i] ?? "");
    onChange({ ...block, columns, rows: rows.map(fit), footer: block.footer?.map(fit), loop: block.loop ? { ...block.loop, cells: fit(block.loop.cells) } : block.loop });
  };
  const grid = (label: string, value: string[][], set: (v: string[][]) => void, addLabel: string) => (
    <Field label={label}>
      <div className="space-y-1.5">
        {value.map((r, i) => (
          <div key={i} className="flex items-center gap-1.5">
            {cols.map((_, c) => (
              <Input key={c} value={r[c] ?? ""} onChange={(e) => set(value.map((x, j) => (j === i ? x.map((y, k) => (k === c ? e.target.value : y)) : x)))} aria-label={`${label} ${i + 1}, column ${c + 1}`} className="min-w-0" />
            ))}
            <IconButton size="sm" label="Remove row" onClick={() => set(value.filter((_, j) => j !== i))}>
              <Trash2 className="size-3" />
            </IconButton>
          </div>
        ))}
        {value.length < 50 ? (
          <Button size="sm" variant="ghost" icon={<Plus className="size-3.5" />} onClick={() => set([...value, cols.map(() => "")])}>
            {addLabel}
          </Button>
        ) : null}
      </div>
    </Field>
  );
  return (
    <div className="space-y-3">
      <Field label="Columns">
        <div className="space-y-1.5">
          {cols.map((c, i) => (
            <div key={i} className="flex items-center gap-1.5">
              <Input value={c.label} onChange={(e) => setCols(cols.map((x, j) => (j === i ? { ...x, label: e.target.value } : x)))} placeholder="Heading" aria-label={`Column ${i + 1} heading`} />
              <AlignPicker value={c.align} onChange={(align) => setCols(cols.map((x, j) => (j === i ? { ...x, align } : x)))} />
              <IconButton size="sm" label="Remove column" disabled={cols.length <= 1} onClick={() => setCols(cols.filter((_, j) => j !== i))}>
                <Trash2 className="size-3" />
              </IconButton>
            </div>
          ))}
          {cols.length < 6 ? (
            <Button size="sm" variant="ghost" icon={<Plus className="size-3.5" />} onClick={() => setCols([...cols, { label: "" }])}>
              Add column
            </Button>
          ) : null}
        </div>
      </Field>
      {grid("Rows", rows, (r) => onChange({ ...block, rows: r }), "Add row")}
      <Checkbox
        label="Add a row for each item in the event (flows)"
        description="For order summaries: one row per entry in a list the triggering event carries."
        checked={Boolean(block.loop)}
        onChange={(e) => onChange({ ...block, loop: e.target.checked ? { source: "event.properties.items", cells: cols.map((_, i) => (i === 0 ? "{{ item.name }}" : i === cols.length - 1 ? "{{ item.price }}" : "{{ item.quantity }}")) } : null })}
      />
      {block.loop ? (
        <div className="space-y-2 rounded-md bg-zinc-50 p-2.5">
          <Field label="List in the event" hint="Each entry is `item`, e.g. {{ item.name }}.">
            <Input value={block.loop.source} onChange={(e) => onChange({ ...block, loop: { ...block.loop!, source: e.target.value } })} className="font-mono text-xs" />
          </Field>
          <div className="flex items-center gap-1.5">
            {cols.map((_, c) => (
              <Input key={c} value={block.loop!.cells[c] ?? ""} onChange={(e) => onChange({ ...block, loop: { ...block.loop!, cells: block.loop!.cells.map((x, k) => (k === c ? e.target.value : x)) } })} aria-label={`Item column ${c + 1}`} className="min-w-0 font-mono text-xs" />
            ))}
          </div>
        </div>
      ) : null}
      {grid("Total rows (bold)", block.footer ?? [], (f) => onChange({ ...block, footer: f.length ? f : undefined }), "Add total row")}
    </div>
  );
}

function QuoteFields({ block, onChange }: Props<"quote">) {
  return (
    <div className="space-y-3">
      <Field label="Quote">
        <Textarea rows={3} value={block.text} onChange={(e) => onChange({ ...block, text: e.target.value })} />
      </Field>
      <div className="grid grid-cols-1 gap-3 sm:grid-cols-2">
        <Field label="Name">
          <Input value={block.author ?? ""} onChange={(e) => onChange({ ...block, author: e.target.value || undefined })} />
        </Field>
        <Field label="Detail (optional)" hint="e.g. Customer since 2023">
          <Input value={block.role ?? ""} onChange={(e) => onChange({ ...block, role: e.target.value || undefined })} />
        </Field>
      </div>
      <Field label="Photo (optional)">
        <ImageInput value={block.image ?? ""} onChange={(image) => onChange({ ...block, image: image || undefined })} aria-label="Photo" />
      </Field>
      <Segmented
        size="sm"
        value={block.style ?? "plain"}
        onChange={(style) => onChange({ ...block, style })}
        options={[
          { value: "plain", label: "Bar" },
          { value: "card", label: "Card" },
        ]}
      />
    </div>
  );
}

function HeaderFields({ block, onChange }: Props<"header">) {
  return (
    <div className="space-y-3">
      <p className="text-xs text-zinc-500">Your logo from Settings already shows above every email. Add one here only for a different picture.</p>
      <Field label="Logo (optional)">
        <ImageInput value={block.logo ?? ""} onChange={(logo) => onChange({ ...block, logo: logo || undefined })} aria-label="Logo" />
      </Field>
      {block.logo ? (
        <div className="grid grid-cols-1 gap-3 sm:grid-cols-2">
          <Field label="Logo alt text">
            <Input value={block.logo_alt ?? ""} onChange={(e) => onChange({ ...block, logo_alt: e.target.value || undefined })} />
          </Field>
          <Field label="Logo link">
            <Input value={block.logo_href ?? ""} onChange={(e) => onChange({ ...block, logo_href: e.target.value || undefined })} placeholder="https://" />
          </Field>
        </div>
      ) : null}
      <Field label="Links">
        <LinkRows links={block.links ?? []} onChange={(links) => onChange({ ...block, links })} />
      </Field>
    </div>
  );
}

function NavFields({ block, onChange }: Props<"nav">) {
  const { account } = useSession();
  return (
    <div className="space-y-3">
      <LinkRows links={block.links} onChange={(links) => onChange({ ...block, links })} />
      <div className="grid grid-cols-1 gap-3 sm:grid-cols-2">
        <Field label="Background" hint="Blank = brand color">
          <ColorInput value={block.background ?? ""} placeholder={account.brand_color} onChange={(background) => onChange({ ...block, background: background || undefined })} />
        </Field>
        <Field label="Link color">
          <ColorInput value={block.color ?? "#ffffff"} onChange={(color) => onChange({ ...block, color })} />
        </Field>
      </div>
      <Checkbox label="Separate links with a line" checked={Boolean(block.separator)} onChange={(e) => onChange({ ...block, separator: e.target.checked })} />
    </div>
  );
}

function FooterFields({ block, onChange }: Props<"footer">) {
  return (
    <div className="space-y-3">
      <Field label="Text">
        <Textarea rows={2} value={block.text ?? ""} onChange={(e) => onChange({ ...block, text: e.target.value || undefined })} />
      </Field>
      <Field label="Links">
        <LinkRows links={block.links ?? []} onChange={(links) => onChange({ ...block, links })} />
      </Field>
      <Checkbox label="Show social icons" checked={block.show_social ?? true} onChange={(e) => onChange({ ...block, show_social: e.target.checked })} />
      {block.show_social ?? true ? (
        <>
          <SocialRows links={block.social ?? []} onChange={(social) => onChange({ ...block, social: social.length ? social : undefined })} />
          <Segmented size="sm" value={block.style ?? "light"} onChange={(style) => onChange({ ...block, style })} options={iconStyles} />
        </>
      ) : null}
      <p className="text-xs text-zinc-500">Your mailing address and unsubscribe link are always added below.</p>
    </div>
  );
}

function ImageTextFields({ block, onChange }: Props<"image_text">) {
  return (
    <div className="space-y-3">
      <Field label="Image">
        <ImageInput value={block.src} onChange={(src) => onChange({ ...block, src })} aria-label="Image" />
      </Field>
      <div className="grid grid-cols-1 gap-3 sm:grid-cols-2">
        <Field label="Alt text" error={block.src && !block.alt?.trim() ? "Describe the image for people who can't see it" : undefined}>
          <Input value={block.alt ?? ""} onChange={(e) => onChange({ ...block, alt: e.target.value || undefined })} />
        </Field>
        <Field label="Image link (optional)">
          <Input value={block.href ?? ""} onChange={(e) => onChange({ ...block, href: e.target.value || undefined })} placeholder="https://" />
        </Field>
      </div>
      <Field label="Heading">
        <Input value={block.heading ?? ""} onChange={(e) => onChange({ ...block, heading: e.target.value || undefined })} />
      </Field>
      <Field label="Text">
        <Textarea rows={3} value={block.html ?? ""} onChange={(e) => onChange({ ...block, html: e.target.value || undefined })} className="font-mono text-xs" />
      </Field>
      <div className="grid grid-cols-1 gap-3 sm:grid-cols-2">
        <Field label="Button label (optional)">
          <Input value={block.button_text ?? ""} onChange={(e) => onChange({ ...block, button_text: e.target.value || undefined })} />
        </Field>
        <Field label="Button link">
          <Input value={block.button_href ?? ""} onChange={(e) => onChange({ ...block, button_href: e.target.value || undefined })} placeholder="https://" />
        </Field>
      </div>
      <div className="flex flex-wrap items-center gap-2">
        <Segmented
          size="sm"
          value={block.image_position ?? "left"}
          onChange={(image_position) => onChange({ ...block, image_position })}
          options={[
            { value: "left", label: "Image left" },
            { value: "right", label: "Image right" },
          ]}
        />
        <Segmented
          size="sm"
          value={String(block.image_width ?? 40) as "33" | "40" | "50"}
          onChange={(v) => onChange({ ...block, image_width: Number(v) as 33 | 40 | 50 })}
          options={[
            { value: "33", label: "⅓" },
            { value: "40", label: "40%" },
            { value: "50", label: "½" },
          ]}
        />
      </div>
    </div>
  );
}

/** Field editor for the content module's block types. */
export function ContentBlockFields({ block, onChange }: { block: ContentBlock; onChange: (b: EmailBlock) => void }) {
  switch (block.type) {
    case "social":
      return <SocialFields block={block} onChange={onChange} />;
    case "video":
      return <VideoFields block={block} onChange={onChange} />;
    case "countdown":
      return <CountdownFields block={block} onChange={onChange} />;
    case "table":
      return <TableFields block={block} onChange={onChange} />;
    case "quote":
      return <QuoteFields block={block} onChange={onChange} />;
    case "header":
      return <HeaderFields block={block} onChange={onChange} />;
    case "nav":
      return <NavFields block={block} onChange={onChange} />;
    case "footer":
      return <FooterFields block={block} onChange={onChange} />;
    case "image_text":
      return <ImageTextFields block={block} onChange={onChange} />;
    case "saved":
      return <SavedRefInfo block={block} />;
  }
}

function SavedRefInfo({ block }: { block: Of<"saved"> }) {
  return (
    <p className="text-xs text-zinc-500">
      {block.frozen
        ? "This copy was taken when the campaign was sent."
        : "This shows the saved block's current content. Edit it on the Saved blocks page to change every email that uses it, or detach it to edit a copy here."}
    </p>
  );
}
