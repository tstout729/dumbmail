import { useState } from "react";
import { Loader2, Monitor, Smartphone } from "lucide-react";
import type { EmailContent } from "../../api";
import { useEmailRender } from "../../api";
import { useDebouncedValue } from "../../lib/hooks";
import { Segmented, cx } from "../ui";

type Props = { content: EmailContent; fromName?: string; className?: string };

export function EmailPreview({ content, fromName, className }: Props) {
  const [device, setDevice] = useState<"desktop" | "mobile">("desktop");
  const debounced = useDebouncedValue(content, 450);
  const render = useEmailRender(debounced);
  const html = render.data?.html;
  const stale = render.isFetching || debounced !== content;

  return (
    <div className={cx("flex min-h-0 flex-col", className)}>
      <div className="flex items-center justify-between gap-2 pb-2">
        <div className="flex items-center gap-2 text-xs text-zinc-500">
          Preview
          {stale ? <Loader2 className="size-3 animate-spin" /> : null}
        </div>
        <Segmented
          size="sm"
          value={device}
          onChange={setDevice}
          options={[
            { value: "desktop", label: <><Monitor className="size-3.5" /> Desktop</> },
            { value: "mobile", label: <><Smartphone className="size-3.5" /> Mobile</> },
          ]}
        />
      </div>
      <div className="min-h-0 flex-1 overflow-auto rounded-lg border border-zinc-200 bg-zinc-100 p-3">
        <div className={cx("mx-auto overflow-hidden rounded-lg bg-white shadow-sm transition-[width]", device === "mobile" ? "w-[375px] max-w-full" : "w-full max-w-[680px]")}>
          <div className="border-b border-zinc-100 px-4 py-2.5">
            <div className="text-xs font-semibold text-zinc-900">{content.from_name || fromName || "Your brand"}</div>
            <div className="truncate text-xs text-zinc-800">{render.data?.subject || content.subject || "(no subject)"}</div>
            <div className="truncate text-[11px] text-zinc-500">{render.data?.preheader || content.preheader || ""}</div>
          </div>
          {render.error ? (
            <div className="px-4 py-10 text-center text-sm text-red-600">{render.error.message}</div>
          ) : html ? (
            <iframe title="Email preview" srcDoc={html} sandbox="allow-popups allow-popups-to-escape-sandbox" className="block h-[640px] w-full border-0" />
          ) : (
            <div className="space-y-3 p-6">
              <div className="pl-skeleton h-6 w-2/3 rounded" />
              <div className="pl-skeleton h-4 w-full rounded" />
              <div className="pl-skeleton h-4 w-5/6 rounded" />
              <div className="pl-skeleton mx-auto h-9 w-32 rounded" />
            </div>
          )}
        </div>
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
