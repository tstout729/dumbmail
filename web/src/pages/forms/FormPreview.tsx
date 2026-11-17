import { useEffect, useRef, useState } from "react";
import { AlertTriangle, Monitor, RotateCcw, Smartphone } from "lucide-react";
import type { FormConfig, FormKind, VersionId } from "../../api";
import { IconButton, Segmented, cx } from "../../components/ui";
import { formKinds } from "./FormsPage";
import { versionLabel } from "./FormAbTest";
import { configForSave, hasPhoneField } from "./formConfig";
import { isOverlayKind } from "./editorShared";

// The shared renderer in /client/form-render.js (src/form_render.js) draws the form on websites, the hosted page and here.
type RenderScreen = "form" | "phone_step" | "success";
type RenderForm = { id: string; kind: FormKind; config: FormConfig; version: VersionId };
type Mounted = { host: HTMLElement | null; close(): void; destroy(): void; setScreen(name: RenderScreen): void };
type FormRenderer = {
  mount(form: RenderForm, opts: { container: HTMLElement; screen?: RenderScreen; preview?: boolean; navigate?: (url: string) => void }): Mounted;
  teaser(form: RenderForm, opts: { container: HTMLElement; preview?: boolean; onOpen?: () => void; onDismiss?: () => void }): { host: HTMLElement | null; destroy(): void };
};
type PreviewScreen = RenderScreen | "teaser";

type FrameWindow = Window & { DumbMailFormRenderer?: FormRenderer };

/** The renderer the frame loaded, or null while loading or when the script failed. */
function rendererIn(frame: HTMLIFrameElement | null) {
  const r = (frame?.contentWindow as FrameWindow | null)?.DumbMailFormRenderer;
  return r && typeof r.mount === "function" && typeof r.teaser === "function" ? r : null;
}

/** A plain stand-in website behind the form, with slots for each kind. Built once so the frame never reloads. */
function frameDoc(origin: string) {
  return `<!doctype html><html><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1">
<style>
html,body{margin:0;min-height:100%;height:100%}
body{font-family:ui-sans-serif,system-ui,-apple-system,"Segoe UI",Roboto,sans-serif;background:#fff;color:#18181b;overflow:auto}
.site{padding:20px 24px 40px;max-width:1100px;margin:0 auto;user-select:none}
.bar{display:flex;align-items:center;justify-content:space-between;margin-bottom:28px}
.logo{height:16px;width:96px;border-radius:4px;background:#e4e4e7}
.nav{display:flex;gap:12px}.nav span{height:10px;width:48px;border-radius:4px;background:#f4f4f5}
.h{height:30px;width:62%;border-radius:6px;background:#e4e4e7;margin-bottom:12px}
.l{height:10px;border-radius:4px;background:#f4f4f5;margin-bottom:8px}
.grid{display:grid;grid-template-columns:repeat(3,minmax(0,1fr));gap:16px;margin-top:28px}
.grid div{height:140px;border-radius:10px;background:#f4f4f5}
#dm-embed{margin:28px auto 0;display:flex;justify-content:center}
#dm-page{display:none;min-height:100%}
#dm-layer{position:fixed;inset:0;pointer-events:none}
#dm-layer>*{pointer-events:auto}
body[data-kind=page] .site{display:none}
body[data-kind=page] #dm-page{display:block;height:100%}
.dm-preview-error{font:13px/1.5 ui-sans-serif,system-ui,sans-serif;color:#b91c1c;background:#fef2f2;border:1px solid #fecaca;border-radius:8px;padding:10px 12px;margin:16px}
</style></head><body>
<div class="site" aria-hidden="true">
<div class="bar"><div class="logo"></div><div class="nav"><span></span><span></span><span></span></div></div>
<div class="h"></div><div class="l" style="width:100%"></div><div class="l" style="width:82%"></div>
<div id="dm-embed"></div>
<div class="grid"><div></div><div></div><div></div></div>
<div class="grid"><div></div><div></div><div></div></div>
</div>
<div id="dm-page"></div>
<div id="dm-layer"></div>
<script src="${origin}/client/form-render.js"></script>
</body></html>`;
}

const srcDoc = frameDoc(window.location.origin);

type FormPreviewProps = {
  formId: string;
  kind: FormKind;
  /** The draft config with the previewed version's content already applied. */
  config: FormConfig;
  version: VersionId;
  testing: boolean;
  /** Stands in for `brand_color` when the form has none, as on the live site. */
  accountBrandColor: string;
};

export function FormPreview({ formId, kind, config, version, testing, accountBrandColor }: FormPreviewProps) {
  const [device, setDevice] = useState<"desktop" | "mobile">("desktop");
  const [screen, setScreen] = useState<PreviewScreen>("form");
  const frameRef = useRef<HTMLIFrameElement>(null);
  const [frame, setFrame] = useState<"loading" | "ready" | "error">("loading");
  const [replay, setReplay] = useState(0);

  const phoneStep = Boolean(config.phone_step?.enabled) && !hasPhoneField(config);
  const teaser = isOverlayKind(kind) && Boolean(config.teaser?.enabled);
  const screens: { value: PreviewScreen; label: string }[] = [
    { value: "form", label: "Form" },
    ...(phoneStep ? [{ value: "phone_step" as const, label: "Phone step" }] : []),
    { value: "success", label: "Thank you" },
    ...(teaser ? [{ value: "teaser" as const, label: "Teaser" }] : []),
  ];
  // A screen that was turned off falls back to the form.
  const shown: PreviewScreen = screens.some((s) => s.value === screen) ? screen : "form";

  // What the renderer gets: the saved shape of the draft (empty keys dropped, so defaults apply), without the test list.
  const { variants: _variants, ...saved } = configForSave(config, {});
  const previewConfig: FormConfig = { ...saved, brand_color: saved.brand_color || accountBrandColor };
  const formJson = JSON.stringify({ id: formId, kind, config: previewConfig, version } satisfies RenderForm);

  // External system: the shared form renderer running in the preview frame. Every change unmounts the old drawing and
  // mounts a fresh one with the new config and screen; cleanup destroys it.
  useEffect(() => {
    const renderer = frame === "ready" ? rendererIn(frameRef.current) : null;
    const doc = frameRef.current?.contentDocument;
    if (!renderer || !doc) return;
    const form = JSON.parse(formJson) as RenderForm;
    doc.body.dataset.kind = form.kind;
    const container = doc.getElementById(form.kind === "page" ? "dm-page" : form.kind === "embed" ? "dm-embed" : "dm-layer");
    if (!container) return;
    let destroy = () => {};
    try {
      if (shown === "teaser") {
        const t = renderer.teaser(form, { container, preview: true, onOpen: () => setScreen("form"), onDismiss: () => {} });
        destroy = () => t.destroy();
      } else {
        const m = renderer.mount(form, { container, screen: shown, preview: true, navigate: () => {} });
        destroy = () => m.destroy();
      }
    } catch (err) {
      container.replaceChildren();
      const box = doc.createElement("div");
      box.className = "dm-preview-error";
      box.textContent = `The preview couldn't draw this form: ${err instanceof Error ? err.message : String(err)}`;
      container.append(box);
      destroy = () => box.remove();
    }
    return () => destroy();
  }, [frame, formJson, shown, replay]);


  return (
    <div className="flex min-h-0 flex-col">
      <div className="mb-2 flex flex-wrap items-center justify-between gap-2">
        <span className="text-xs text-zinc-500">
          Preview · {formKinds.find((k) => k.value === kind)?.label}
          {testing ? ` · ${versionLabel(version)}` : ""}
        </span>
        <div className="flex flex-wrap items-center gap-2">
          <Segmented size="sm" value={shown} onChange={setScreen} options={screens} />
          <Segmented
            size="sm"
            value={device}
            onChange={setDevice}
            options={[
              { value: "desktop", label: <><Monitor className="size-3.5" /><span className="sr-only">Desktop</span></> },
              { value: "mobile", label: <><Smartphone className="size-3.5" /><span className="sr-only">Mobile</span></> },
            ]}
          />
          <IconButton size="sm" label="Replay preview" onClick={() => setReplay((n) => n + 1)}>
            <RotateCcw className="size-3.5" />
          </IconButton>
        </div>
      </div>
      <div className="flex justify-center rounded-lg border border-zinc-200 bg-zinc-100 p-3 sm:p-4">
        <div
          className={cx(
            "relative h-[640px] max-h-[calc(100dvh-12rem)] min-h-[480px] overflow-hidden rounded-lg bg-white shadow-sm ring-1 ring-zinc-950/5 transition-[width]",
            device === "mobile" ? "w-[375px] max-w-full" : "w-full",
          )}
        >
          <iframe
            ref={frameRef}
            title="Form preview"
            srcDoc={srcDoc}
            onLoad={(e) => setFrame(rendererIn(e.currentTarget) ? "ready" : "error")}
            className="absolute inset-0 size-full border-0" />
          {frame === "error" ? (
            <div className="absolute inset-x-4 top-4 flex items-start gap-2 rounded-md border border-amber-200 bg-amber-50 px-3 py-2 text-sm text-amber-900">
              <AlertTriangle className="mt-0.5 size-4 shrink-0" />
              <span>The preview couldn&rsquo;t load the form renderer. Check your connection and reload the page. Your settings still save.</span>
            </div>
          ) : null}
        </div>
      </div>
      <p className="mt-2 text-xs text-zinc-500">
        This is the same code your website runs. Sign-ups in the preview aren&rsquo;t saved.
      </p>
    </div>
  );
}
