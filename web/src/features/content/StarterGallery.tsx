import { FilePlus2 } from "lucide-react";
import type { Template } from "../../api/types";
import { Badge, Skeleton, cx } from "../../components/ui";
import { contentApi, useStarters } from "./api";
import type { Starter } from "./types";

function Thumb({ html }: { html?: string }) {
  return (
    <div className="pointer-events-none relative h-40 overflow-hidden bg-zinc-50">
      {html ? (
        <iframe
          title="Starter preview"
          srcDoc={html}
          sandbox=""
          tabIndex={-1}
          loading="lazy"
          className="absolute left-0 top-0 h-[900px] w-[640px] origin-top-left scale-[0.36] border-0 sm:scale-[0.4]"
        />
      ) : (
        <div className="space-y-2 p-4">
          <div className="pl-skeleton h-4 w-2/3 rounded" />
          <div className="pl-skeleton h-3 w-full rounded" />
        </div>
      )}
    </div>
  );
}

type Props = {
  onPick: (s: Starter) => void;
  /** Highlighted card ("blank" for the blank card). */
  selected?: string | null;
  /** Show a "Blank" card first. */
  blank?: boolean;
  className?: string;
};

/** Starter designs in the account's brand, with real rendered previews. */
export function StarterGallery({ onPick, selected, blank, className }: Props) {
  const starters = useStarters(true);
  const card = (selectedNow: boolean) =>
    cx(
      "group overflow-hidden rounded-lg border bg-white text-left shadow-xs transition-shadow hover:shadow-md",
      selectedNow ? "border-accent-500 ring-2 ring-accent-200" : "border-zinc-200 hover:border-zinc-300",
    );
  return (
    <div className={cx("grid grid-cols-1 gap-3 sm:grid-cols-2 lg:grid-cols-3", className)}>
      {blank ? (
        <button type="button" onClick={() => onPick({ key: "blank", name: "Blank", description: "", kind: "campaign", trigger: null })} className={card(selected === "blank")}>
          <div className="flex h-40 items-center justify-center bg-zinc-50 text-zinc-400">
            <FilePlus2 className="size-8" />
          </div>
          <div className="border-t border-zinc-100 px-3 py-2">
            <div className="text-sm font-medium text-zinc-900">Blank</div>
            <div className="text-[11px] text-zinc-500">A heading, text and a button.</div>
          </div>
        </button>
      ) : null}
      {starters.isPending
        ? [0, 1, 2, 3, 4, 5].map((i) => <Skeleton key={i} className="h-56" />)
        : starters.error
          ? <p className="text-sm text-red-600">{starters.error.message}</p>
          : (starters.data ?? []).map((s) => (
              <button key={s.key} type="button" onClick={() => onPick(s)} className={card(selected === s.key)}>
                <Thumb html={s.html} />
                <div className="border-t border-zinc-100 px-3 py-2">
                  <div className="flex items-center gap-1.5">
                    <span className="text-sm font-medium text-zinc-900 group-hover:text-accent-800">{s.name}</span>
                    {s.kind === "flow" ? <Badge tone="gray">Flow</Badge> : null}
                  </div>
                  <div className="line-clamp-2 text-[11px] text-zinc-500">{s.description}</div>
                </div>
              </button>
            ))}
    </div>
  );
}

/** A starter's content as a template-shaped value, for pickers that load templates. */
export async function starterAsTemplate(key: string): Promise<Template> {
  const s = await contentApi.starter(key);
  const now = new Date().toISOString();
  return { id: "", name: s.name, channel: "email", content: s.content, created_at: now, updated_at: now };
}
