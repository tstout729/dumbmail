import { useState } from "react";
import { ChevronDown, Paintbrush } from "lucide-react";
import type { FlyoutPosition, FormAnimation, FormFont, FormShadow, FormStyle, HeadlineWeight, PopupPosition } from "../../api";
import { Button, Checkbox, CollapsibleCard, ColorInput, ConfirmDialog, Field, RangeInput, Segmented, Select, SubHeading, Textarea, Toggle, cx } from "../../components/ui";
import { animations, cssHooks, flyoutPositions, fonts, isHex, kindStyleDefaults, layouts, popupPositions, readableOn, styleDefaults, tint } from "./formConfig";
import { ResettableLabel, Row, isOverlayKind, type Editor } from "./editorShared";

type SectionProps = { ed: Editor; open: boolean; onToggle: () => void };

type ColorKey =
  | "background_color"
  | "headline_color"
  | "text_color"
  | "button_color"
  | "button_text_color"
  | "input_background"
  | "input_border_color"
  | "input_text_color"
  | "border_color"
  | "overlay_color"
  | "close_color"
  | "page_background";

type NumberKey =
  | "width"
  | "headline_size"
  | "body_size"
  | "corner_radius"
  | "button_radius"
  | "input_radius"
  | "padding"
  | "border_width"
  | "overlay_opacity";

/** The little picture on each layout tile: where the image (filled) sits next to the text (lines). */
function LayoutGlyph({ layout }: { layout: FormStyle["layout"] }) {
  const lines = (
    <span className="flex flex-1 flex-col justify-center gap-0.5 p-0.5">
      <span className="h-0.5 w-3/4 rounded bg-current" />
      <span className="h-0.5 w-full rounded bg-current opacity-50" />
      <span className="mt-0.5 h-1 w-full rounded-sm bg-current opacity-70" />
    </span>
  );
  const img = <span className="rounded-sm bg-current opacity-25" />;
  return (
    <span className="relative flex h-9 w-12 overflow-hidden rounded border border-current/30">
      {layout === "background" ? (
        <>
          <span className="absolute inset-0 bg-current opacity-20" />
          <span className="relative flex flex-1 px-1">{lines}</span>
        </>
      ) : layout === "image_left" || layout === "image_right" ? (
        <span className={cx("flex flex-1 gap-0.5 p-0.5", layout === "image_right" && "flex-row-reverse")}>
          <span className="flex w-1/2">{img}</span>
          {lines}
        </span>
      ) : (
        <span className="flex flex-1 flex-col gap-0.5 p-0.5">
          <span className="flex h-3">{img}</span>
          {lines}
        </span>
      )}
    </span>
  );
}

export function StyleSection({ ed, open, onToggle }: SectionProps) {
  const s: FormStyle = ed.config.style ?? {};
  const kind = ed.kind;
  const overlay = isOverlayKind(kind);
  const kd = kindStyleDefaults(kind);
  const brand = isHex(ed.config.brand_color) ? ed.config.brand_color : ed.accountBrandColor;
  const button = isHex(s.button_color) ? s.button_color : brand;
  const [confirmReset, setConfirmReset] = useState(false);
  const [cssOpen, setCssOpen] = useState(Boolean(s.custom_css));
  const set = (patch: Partial<FormStyle>) => ed.set({ style: { ...s, ...patch } });

  const color = (key: ColorKey, label: string, placeholder: string, hint?: string) => (
    <Row label={label} hint={hint} onReset={s[key] ? () => set({ [key]: undefined }) : undefined}>
      <ColorInput value={s[key] ?? ""} placeholder={placeholder} onChange={(v) => set({ [key]: v || undefined })} aria-label={label} />
    </Row>
  );
  const range = (key: NumberKey, label: string, min: number, max: number, placeholder: number, unit = "px", hint?: string) => (
    <Field label={<ResettableLabel onReset={s[key] !== undefined ? () => set({ [key]: undefined }) : undefined}>{label}</ResettableLabel>} hint={hint}>
      <RangeInput value={s[key]} placeholder={placeholder} min={min} max={max} unit={unit} onChange={(v) => set({ [key]: v })} aria-label={label} />
    </Field>
  );

  const layout = s.layout ?? styleDefaults.layout;
  const font = fonts.find((f) => f.value === (s.font ?? styleDefaults.font))?.label ?? "System";
  const customized = Object.values(s).some((v) => v !== undefined && v !== "");

  return (
    <CollapsibleCard
      title="Style"
      icon={<Paintbrush />}
      summary={`${layouts.find((l) => l.value === layout)?.label} · ${s.width ?? kd.width}px wide · ${font.replace(/ \(.*\)$/, "")}`}
      open={open}
      onToggle={onToggle}
      actions={
        customized ? (
          <Button size="sm" variant="ghost" onClick={() => setConfirmReset(true)}>
            Reset style
          </Button>
        ) : null
      }
    >
      <SubHeading>Layout</SubHeading>
      <div className="grid grid-cols-4 gap-2" role="radiogroup" aria-label="Layout">
        {layouts.map((l) => (
          <button
            key={l.value}
            type="button"
            role="radio"
            aria-checked={layout === l.value}
            onClick={() => set({ layout: l.value })}
            className={cx(
              "flex flex-col items-center gap-1.5 rounded-md border px-1 py-2 text-[11px] font-medium",
              layout === l.value ? "border-accent-600 bg-accent-50/60 text-accent-800 ring-1 ring-accent-600" : "border-zinc-200 text-zinc-600 hover:border-zinc-300",
            )}
          >
            <LayoutGlyph layout={l.value} />
            {l.label}
          </button>
        ))}
      </div>
      {!ed.config.image_url ? <p className="-mt-2 text-xs text-zinc-500">Layouts place the image. Add one under Content to see the difference.</p> : null}
      {range("width", "Width", 280, 900, kd.width, "px", "Never wider than the screen.")}
      <div className="flex flex-wrap gap-x-6 gap-y-3">
        <div className="space-y-1">
          <div className="text-xs font-medium text-zinc-700">Text alignment</div>
          <Segmented
            value={s.align ?? styleDefaults.align}
            onChange={(align) => set({ align })}
            options={[
              { value: "center", label: "Center" },
              { value: "left", label: "Left" },
            ]}
          />
        </div>
        {kind === "popup" ? (
          <div className="space-y-1">
            <div className="text-xs font-medium text-zinc-700">Position</div>
            <Segmented
              value={(s.position as PopupPosition | undefined) ?? "center"}
              onChange={(position: PopupPosition) => set({ position })}
              options={popupPositions}
            />
          </div>
        ) : null}
      </div>
      {kind === "flyout" ? (
        <div className="space-y-1">
          <div className="text-xs font-medium text-zinc-700">Corner</div>
          <div className="grid w-56 grid-cols-2 gap-1.5" role="radiogroup" aria-label="Corner">
            {flyoutPositions.map((p) => {
              const on = ((s.position as FlyoutPosition | undefined) ?? "bottom_right") === p.value;
              return (
                <button
                  key={p.value}
                  type="button"
                  role="radio"
                  aria-checked={on}
                  onClick={() => set({ position: p.value })}
                  className={cx(
                    "rounded-md border px-2 py-1.5 text-xs font-medium",
                    on ? "border-accent-600 bg-accent-50/60 text-accent-800 ring-1 ring-accent-600" : "border-zinc-200 text-zinc-600 hover:border-zinc-300",
                  )}
                >
                  {p.label}
                </button>
              );
            })}
          </div>
        </div>
      ) : null}
      {overlay ? (
        <Field label="Animation" hint="Turned off for visitors who prefer less motion.">
          <Select value={s.animation ?? styleDefaults.animation} onChange={(e) => set({ animation: e.target.value as FormAnimation })} className="w-40">
            {animations.map((a) => (
              <option key={a.value} value={a.value}>
                {a.label}
              </option>
            ))}
          </Select>
        </Field>
      ) : null}

      <SubHeading>Text</SubHeading>
      <Field label="Font" hint={s.font === "inherit" ? "The preview can't show your website's font; it uses a system font." : undefined}>
        <Select value={s.font ?? styleDefaults.font} onChange={(e) => set({ font: e.target.value as FormFont })}>
          {fonts.map((f) => (
            <option key={f.value} value={f.value}>
              {f.label}
            </option>
          ))}
        </Select>
      </Field>
      <div className="grid grid-cols-[minmax(0,1fr)_auto] items-end gap-3">
        {range("headline_size", "Headline size", 14, 56, styleDefaults.headline_size)}
        <Field label="Weight">
          <Select
            value={String(s.headline_weight ?? styleDefaults.headline_weight)}
            onChange={(e) => set({ headline_weight: Number(e.target.value) as HeadlineWeight })}
            className="w-28"
          >
            <option value="400">Regular</option>
            <option value="500">Medium</option>
            <option value="600">Semibold</option>
            <option value="700">Bold</option>
            <option value="800">Extra bold</option>
          </Select>
        </Field>
      </div>
      {range("body_size", "Text size", 12, 22, styleDefaults.body_size)}

      <SubHeading>Colors</SubHeading>
      <div className="space-y-1">
        <Row label="Brand color" hint="Used for the button unless you pick a button color.">
          <ColorInput
            value={ed.config.brand_color ?? ""}
            placeholder={ed.accountBrandColor}
            onChange={(v) => ed.set({ brand_color: v || undefined })}
            aria-label="Brand color"
          />
        </Row>
        {color("background_color", "Background", styleDefaults.background_color)}
        {color("headline_color", "Headline", styleDefaults.headline_color)}
        {color("text_color", "Text", styleDefaults.text_color, "Body, labels and small print.")}
        {kind === "page" ? color("page_background", "Page background", tint(button, 0.9), "Behind the form on the hosted page. Default: a light tint of the button color.") : null}
      </div>

      <SubHeading>Button</SubHeading>
      <div className="space-y-1">
        {color("button_color", "Button color", brand)}
        {color("button_text_color", "Button text", readableOn(button), "Default: white or black, whichever reads better.")}
        <Row label="Full width">
          <Toggle checked={s.button_full_width ?? true} onChange={(on) => set({ button_full_width: on })} label="Full-width button" />
        </Row>
      </div>
      {range("button_radius", "Button corners", 0, 40, styleDefaults.button_radius, "px", "40 makes a pill.")}

      <SubHeading>Inputs</SubHeading>
      <div className="space-y-1">
        {color("input_background", "Background", styleDefaults.input_background)}
        {color("input_border_color", "Border", styleDefaults.input_border_color)}
        {color("input_text_color", "Text", styleDefaults.input_text_color)}
      </div>
      {range("input_radius", "Input corners", 0, 40, styleDefaults.input_radius)}

      <SubHeading>Shape</SubHeading>
      {range("corner_radius", "Form corners", 0, 40, styleDefaults.corner_radius)}
      {range("padding", "Padding", 12, 64, styleDefaults.padding)}
      {range("border_width", "Border", 0, 8, kd.border_width)}
      {(s.border_width ?? kd.border_width) > 0 ? <div className="space-y-1">{color("border_color", "Border color", styleDefaults.border_color)}</div> : null}
      <div className="space-y-1">
        <div className="text-xs font-medium text-zinc-700">Shadow</div>
        <Segmented
          value={s.shadow ?? kd.shadow}
          onChange={(shadow: FormShadow) => set({ shadow })}
          options={[
            { value: "none", label: "None" },
            { value: "soft", label: "Soft" },
            { value: "strong", label: "Strong" },
          ]}
        />
      </div>

      {overlay ? (
        <>
          <SubHeading>{kind === "popup" ? "Close button and backdrop" : "Close button"}</SubHeading>
          <div className="space-y-1">{color("close_color", "Close (×) color", styleDefaults.close_color)}</div>
          {kind === "popup" ? (
            <>
              <div className="space-y-1">{color("overlay_color", "Backdrop color", styleDefaults.overlay_color, "Dims the page behind the popup.")}</div>
              {range("overlay_opacity", "Backdrop darkness", 0, 90, styleDefaults.overlay_opacity, "%")}
              <Checkbox
                label="Close when they click outside the popup"
                checked={s.close_on_overlay ?? true}
                onChange={(e) => set({ close_on_overlay: e.target.checked })}
              />
            </>
          ) : null}
        </>
      ) : null}

      <div className="rounded-md border border-zinc-200">
        <button
          type="button"
          aria-expanded={cssOpen}
          onClick={() => setCssOpen(!cssOpen)}
          className="flex w-full items-center justify-between gap-2 px-3 py-2 text-left text-sm font-medium text-zinc-700"
        >
          Advanced: custom CSS
          <ChevronDown className={cx("size-4 text-zinc-400 transition-transform", cssOpen && "rotate-180")} />
        </button>
        {cssOpen ? (
        <div className="space-y-2 border-t border-zinc-100 p-3">
          <Textarea
            rows={6}
            maxLength={10000}
            spellCheck={false}
            className="font-mono text-xs"
            value={s.custom_css ?? ""}
            placeholder={".dm-button {\n  text-transform: uppercase;\n  letter-spacing: 0.05em;\n}"}
            onChange={(e) => set({ custom_css: e.target.value || undefined })}
            aria-label="Custom CSS"
          />
          <p className="text-xs text-zinc-500">Applies only inside this form; it can&rsquo;t affect the rest of your site. Classes you can style:</p>
          <div className="flex flex-wrap gap-1">
            {cssHooks.map((h) => (
              <code key={h} className="rounded bg-zinc-100 px-1 py-0.5 font-mono text-[11px] text-zinc-700">
                {h}
              </code>
            ))}
          </div>
        </div>
        ) : null}
      </div>
      {confirmReset ? (
        <ConfirmDialog
          title="Reset the style?"
          body="Layout, colors, fonts and sizes go back to the defaults. Nothing changes on your site until you save."
          confirmLabel="Reset style"
          onConfirm={() => {
            ed.set({ style: undefined });
            setConfirmReset(false);
          }}
          onClose={() => setConfirmReset(false)}
        />
      ) : null}
    </CollapsibleCard>
  );
}
