import { useRef } from "react";
import { AlignCenter, AlignLeft, AlignRight, Bold, Italic, Link2, List } from "lucide-react";
import type { Align, EmailBlock } from "../../api";
import { ColorInput, Field, IconButton, Input, Segmented, Select, Textarea } from "../ui";
import { ProductsBlockFields } from "../../features/catalog/ProductsBlockFields";
import { ContentBlockFields } from "../../features/content/ContentBlockFields";
import { isContentBlock } from "../../features/content/blockDefs";
import { VariableMenu, insertAtCursor, wrapSelection } from "./VariableMenu";

type Props<T extends EmailBlock> = { block: T; onChange: (b: EmailBlock) => void };

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

function HeadingFields({ block, onChange }: Props<Extract<EmailBlock, { type: "heading" }>>) {
  const ref = useRef<HTMLInputElement>(null);
  return (
    <div className="space-y-3">
      <div className="flex items-center gap-1.5">
        <Input ref={ref} value={block.text} onChange={(e) => onChange({ ...block, text: e.target.value })} aria-label="Heading text" />
        <VariableMenu compact onInsert={(v) => onChange({ ...block, text: insertAtCursor(ref.current, block.text, v) })} />
      </div>
      <div className="flex flex-wrap items-center gap-2">
        <Segmented
          size="sm"
          value={String(block.level ?? 1) as "1" | "2" | "3"}
          onChange={(v) => onChange({ ...block, level: Number(v) as 1 | 2 | 3 })}
          options={[
            { value: "1", label: "H1" },
            { value: "2", label: "H2" },
            { value: "3", label: "H3" },
          ]}
        />
        <AlignPicker value={block.align} onChange={(align) => onChange({ ...block, align })} />
      </div>
    </div>
  );
}

function TextFields({ block, onChange }: Props<Extract<EmailBlock, { type: "text" }>>) {
  const ref = useRef<HTMLTextAreaElement>(null);
  const wrap = (before: string, after: string) => onChange({ ...block, html: wrapSelection(ref.current, block.html, before, after) });
  return (
    <div className="space-y-2">
      <div className="flex flex-wrap items-center gap-0.5 rounded-md border border-zinc-200 bg-zinc-50 p-0.5">
        <IconButton size="sm" label="Bold" onClick={() => wrap("<strong>", "</strong>")}>
          <Bold className="size-3.5" />
        </IconButton>
        <IconButton size="sm" label="Italic" onClick={() => wrap("<em>", "</em>")}>
          <Italic className="size-3.5" />
        </IconButton>
        <IconButton
          size="sm"
          label="Link"
          onClick={() => {
            const href = window.prompt("Link URL", "https://");
            if (href) wrap(`<a href="${href.replace(/"/g, "&quot;")}">`, "</a>");
          }}
        >
          <Link2 className="size-3.5" />
        </IconButton>
        <IconButton size="sm" label="Bulleted list" onClick={() => wrap("<ul><li>", "</li></ul>")}>
          <List className="size-3.5" />
        </IconButton>
        <span className="mx-1 h-4 w-px bg-zinc-200" />
        <VariableMenu compact onInsert={(v) => onChange({ ...block, html: insertAtCursor(ref.current, block.html, v) })} />
        <div className="ml-auto">
          <AlignPicker value={block.align} onChange={(align) => onChange({ ...block, align })} />
        </div>
      </div>
      <Textarea
        ref={ref}
        rows={5}
        value={block.html}
        onChange={(e) => onChange({ ...block, html: e.target.value })}
        className="font-mono text-xs"
        aria-label="Text HTML"
      />
      <p className="text-[11px] text-zinc-500">Inline HTML: p, b, strong, i, em, a, br, ul, ol, li.</p>
    </div>
  );
}

function ImageFields({ block, onChange }: Props<Extract<EmailBlock, { type: "image" }>>) {
  return (
    <div className="grid grid-cols-1 gap-3 sm:grid-cols-2">
      <Field label="Image URL" className="sm:col-span-2">
        <Input value={block.src} onChange={(e) => onChange({ ...block, src: e.target.value })} placeholder="https://…/image.jpg" />
      </Field>
      <Field label="Alt text" error={block.src && !block.alt?.trim() ? "Describe the image for people who can't see it" : undefined}>
        <Input value={block.alt ?? ""} onChange={(e) => onChange({ ...block, alt: e.target.value || undefined })} />
      </Field>
      <Field label="Width (px)" hint="Blank = full width">
        <Input
          type="number"
          min={20}
          max={600}
          value={block.width ?? ""}
          onChange={(e) => onChange({ ...block, width: e.target.value ? Number(e.target.value) : undefined })}
        />
      </Field>
      <Field label="Link (optional)">
        <Input value={block.href ?? ""} onChange={(e) => onChange({ ...block, href: e.target.value || undefined })} placeholder="https://" />
      </Field>
      <Field label="Rounded corners (px)">
        <Input type="number" min={0} max={40} value={block.radius ?? 6} onChange={(e) => onChange({ ...block, radius: e.target.value === "" ? undefined : Math.min(40, Math.max(0, Number(e.target.value))) })} />
      </Field>
    </div>
  );
}

function ButtonFields({ block, onChange }: Props<Extract<EmailBlock, { type: "button" }>>) {
  return (
    <div className="grid grid-cols-1 gap-3 sm:grid-cols-2">
      <Field label="Label">
        <Input value={block.text} onChange={(e) => onChange({ ...block, text: e.target.value })} />
      </Field>
      <Field label="Link">
        <Input value={block.href} onChange={(e) => onChange({ ...block, href: e.target.value })} />
      </Field>
      <Field label="Color" hint="Blank = brand color">
        <ColorInput value={block.color ?? ""} onChange={(color) => onChange({ ...block, color: color || undefined })} />
      </Field>
      <Field label="Alignment">
        <AlignPicker value={block.align} onChange={(align) => onChange({ ...block, align })} />
      </Field>
    </div>
  );
}

function SpacerFields({ block, onChange }: Props<Extract<EmailBlock, { type: "spacer" }>>) {
  return (
    <Field label={`Height: ${block.height ?? 24}px`}>
      <input
        type="range"
        min={4}
        max={120}
        step={4}
        value={block.height ?? 24}
        onChange={(e) => onChange({ ...block, height: Number(e.target.value) })}
        className="w-full accent-accent-700"
      />
    </Field>
  );
}

function ProductFields({ block, onChange }: Props<Extract<EmailBlock, { type: "product" }>>) {
  return (
    <div className="grid grid-cols-1 gap-3 sm:grid-cols-2">
      <Field label="Title" className="sm:col-span-2">
        <Input value={block.title} onChange={(e) => onChange({ ...block, title: e.target.value })} />
      </Field>
      <Field label="Image URL" className="sm:col-span-2">
        <Input value={block.image ?? ""} onChange={(e) => onChange({ ...block, image: e.target.value || undefined })} />
      </Field>
      <Field label="Price">
        <Input value={block.price ?? ""} onChange={(e) => onChange({ ...block, price: e.target.value || undefined })} />
      </Field>
      <Field label="Button label">
        <Input value={block.button_text ?? ""} onChange={(e) => onChange({ ...block, button_text: e.target.value || undefined })} />
      </Field>
      <Field label="Product link" className="sm:col-span-2">
        <Input value={block.href} onChange={(e) => onChange({ ...block, href: e.target.value })} />
      </Field>
    </div>
  );
}

function HtmlFields({ block, onChange }: Props<Extract<EmailBlock, { type: "html" }>>) {
  return (
    <Textarea
      rows={8}
      value={block.html}
      onChange={(e) => onChange({ ...block, html: e.target.value })}
      className="font-mono text-xs"
      aria-label="Raw HTML"
    />
  );
}

function ColumnsCount({ block, onChange }: Props<Extract<EmailBlock, { type: "columns" }>>) {
  return (
    <Field label="Columns">
      <Select
        value={block.columns.length}
        onChange={(e) => {
          const n = Number(e.target.value);
          const columns = Array.from({ length: n }, (_, i) => block.columns[i] ?? [{ type: "text" as const, html: "<p>New column</p>" }]);
          onChange({ ...block, columns });
        }}
        className="w-24"
      >
        <option value={2}>2</option>
        <option value={3}>3</option>
      </Select>
    </Field>
  );
}

/** Field editor for every block type except the nested content of columns (handled by BlockList). */
export function BlockFields({ block, onChange }: { block: EmailBlock; onChange: (b: EmailBlock) => void }) {
  if (isContentBlock(block)) return <ContentBlockFields block={block} onChange={onChange} />;
  switch (block.type) {
    case "heading":
      return <HeadingFields block={block} onChange={onChange} />;
    case "text":
      return <TextFields block={block} onChange={onChange} />;
    case "image":
      return <ImageFields block={block} onChange={onChange} />;
    case "button":
      return <ButtonFields block={block} onChange={onChange} />;
    case "divider":
      return <p className="text-xs text-zinc-500">A thin horizontal rule. Nothing to configure.</p>;
    case "spacer":
      return <SpacerFields block={block} onChange={onChange} />;
    case "columns":
      return <ColumnsCount block={block} onChange={onChange} />;
    case "product":
      return <ProductFields block={block} onChange={onChange} />;
    case "html":
      return <HtmlFields block={block} onChange={onChange} />;
    case "products":
      return <ProductsBlockFields block={block} onChange={onChange} />;
  }
}
