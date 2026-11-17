import { useState, type ReactNode } from "react";
import { Columns3, FileText, Loader2, Monitor, Moon, Smartphone } from "lucide-react";
import type { EmailContent } from "../../api";
import { useEmailRender } from "../../api";
import { useDebouncedValue } from "../../lib/hooks";
import { PreviewAsPicker } from "../../features/catalog/PreviewAsPicker";
import { Segmented, cx } from "../ui";

type Props = { content: EmailContent; fromName?: string; className?: string };
type View = "desktop" | "mobile" | "dark" | "all" | "text";

/**
 * Dark mode as the Gmail and Outlook apps do it to emails without their own dark styles: colors
 * inverted, photos left alone. Apple Mail keeps this email light (it declares a light color scheme).
 */
function darkened(html: string): string {
  const css = "<style>html{filter:invert(1) hue-rotate(180deg);background:#fff}img{filter:invert(1) hue-rotate(180deg)}</style>";
  return html.includes("</head>") ? html.replace("</head>", `${css}</head>`) : css + html;
}

function Frame({ html, title, className }: { html: string; title: string; className?: string }) {
  return <iframe title={title} srcDoc={html} sandbox="allow-popups allow-popups-to-escape-sandbox" className={cx("block w-full border-0", className)} />;
}

function Inbox({ from, subject, preheader, dark, children, className }: { from: string; subject: string; preheader: string; dark?: boolean; children: ReactNode; className?: string }) {
  return (
    <div className={cx("overflow-hidden rounded-lg shadow-sm", dark ? "bg-zinc-900" : "bg-white", className)}>
      <div className={cx("border-b px-4 py-2.5", dark ? "border-zinc-800" : "border-zinc-100")}>
        <div className={cx("text-xs font-semibold", dark ? "text-zinc-100" : "text-zinc-900")}>{from}</div>
        <div className={cx("truncate text-xs", dark ? "text-zinc-200" : "text-zinc-800")}>{subject || "(no subject)"}</div>
        <div className="truncate text-[11px] text-zinc-500">{preheader}</div>
      </div>
      {children}
    </div>
  );
}

export function EmailPreview({ content, fromName, className }: Props) {
  const [view, setView] = useState<View>("desktop");
  const [profileId, setProfileId] = useState<string | undefined>();
  const debounced = useDebouncedValue(content, 450);
  const render = useEmailRender(debounced, profileId);
  const html = render.data?.html;
  const stale = render.isFetching || debounced !== content;
  const from = content.from_name || fromName || "Your brand";
  const subject = render.data?.subject || content.subject || "";
  const preheader = render.data?.preheader || content.preheader || "";

  const body = (h: string, v: Exclude<View, "all" | "text">, height = "h-[640px]") => (
    <Inbox from={from} subject={subject} preheader={preheader} dark={v === "dark"}>
      <Frame html={v === "dark" ? darkened(h) : h} title={`Email preview (${v})`} className={height} />
    </Inbox>
  );

  return (
    <div className={cx("flex min-h-0 flex-col", className)}>
      <div className="flex flex-wrap items-center justify-between gap-2 pb-2">
        <div className="flex min-w-0 items-center gap-2 text-xs text-zinc-500">
          <PreviewAsPicker onChange={setProfileId} />
          {stale ? <Loader2 className="size-3 shrink-0 animate-spin" /> : null}
        </div>
        <Segmented
          size="sm"
          value={view}
          onChange={setView}
          options={[
            { value: "desktop", label: <><Monitor className="size-3.5" /> Desktop</> },
            { value: "mobile", label: <><Smartphone className="size-3.5" /> Phone</> },
            { value: "dark", label: <><Moon className="size-3.5" /> Dark</> },
            { value: "all", label: <><Columns3 className="size-3.5" /> All</> },
            { value: "text", label: <><FileText className="size-3.5" /> Text</> },
          ]}
        />
      </div>
      <div className="min-h-0 flex-1 overflow-auto rounded-lg border border-zinc-200 bg-zinc-100 p-3">
        {render.error ? (
          <div className="rounded-lg bg-white px-4 py-10 text-center text-sm text-red-600">{render.error.message}</div>
        ) : !html ? (
          <div className="mx-auto max-w-[680px] space-y-3 rounded-lg bg-white p-6">
            <div className="pl-skeleton h-6 w-2/3 rounded" />
            <div className="pl-skeleton h-4 w-full rounded" />
            <div className="pl-skeleton h-4 w-5/6 rounded" />
            <div className="pl-skeleton mx-auto h-9 w-32 rounded" />
          </div>
        ) : view === "text" ? (
          <Inbox from={from} subject={subject} preheader={preheader} className="mx-auto max-w-[680px]">
            <pre className="whitespace-pre-wrap break-words px-4 py-4 font-mono text-xs leading-relaxed text-zinc-800">{render.data?.text}</pre>
          </Inbox>
        ) : view === "all" ? (
          <div className="flex gap-3 overflow-x-auto pb-1">
            <div className="w-[600px] shrink-0">
              <div className="pb-1 text-[11px] font-medium text-zinc-500">Desktop</div>
              {body(html, "desktop")}
            </div>
            <div className="w-[375px] shrink-0">
              <div className="pb-1 text-[11px] font-medium text-zinc-500">Phone</div>
              {body(html, "mobile")}
            </div>
            <div className="w-[375px] shrink-0">
              <div className="pb-1 text-[11px] font-medium text-zinc-500">Phone, dark mode</div>
              {body(html, "dark")}
            </div>
          </div>
        ) : (
          <div className={cx("mx-auto transition-[width]", view === "desktop" ? "w-full max-w-[680px]" : "w-[375px] max-w-full")}>
            {body(html, view)}
            {view === "dark" ? <p className="pt-2 text-center text-[11px] text-zinc-500">How the Gmail and Outlook apps show it in dark mode. Apple Mail keeps it light.</p> : null}
          </div>
        )}
      </div>
    </div>
  );
}

/** Small, non-interactive scaled preview used for template thumbnails. */
export function EmailThumbnail({ content }: { content: EmailContent }) {
  const render = useEmailRender(content);
  return (
    <div className="pointer-events-none relative h-44 overflow-hidden bg-zinc-50">
      {render.data?.html ? (
        <iframe
          title="Template thumbnail"
          srcDoc={render.data.html}
          sandbox=""
          tabIndex={-1}
          loading="lazy"
          className="absolute left-0 top-0 h-[880px] w-[600px] origin-top-left scale-[0.4] border-0 sm:scale-[0.45]"
        />
      ) : (
        <div className="space-y-2 p-4">
          <div className="pl-skeleton h-4 w-2/3 rounded" />
          <div className="pl-skeleton h-3 w-full rounded" />
          <div className="pl-skeleton h-3 w-4/5 rounded" />
        </div>
      )}
    </div>
  );
}
