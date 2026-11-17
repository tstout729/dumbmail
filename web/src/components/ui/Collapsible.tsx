import type { ReactNode } from "react";
import { ChevronDown } from "lucide-react";
import { cx } from "./cx";

type CollapsibleCardProps = {
  title: ReactNode;
  /** Shown under the title while collapsed: a one-line summary of the settings inside. */
  summary?: ReactNode;
  icon?: ReactNode;
  open: boolean;
  onToggle: () => void;
  /** Controls in the header (e.g. version tabs); clicks on them don't toggle the card. */
  actions?: ReactNode;
  children: ReactNode;
};

/** A settings card that folds to its title, for long editors. */
export function CollapsibleCard({ title, summary, icon, open, onToggle, actions, children }: CollapsibleCardProps) {
  return (
    <section className={cx("rounded-lg border bg-white shadow-xs", open ? "border-zinc-300" : "border-zinc-200")}>
      <header className="flex items-center gap-2 px-4 py-3">
        <button type="button" onClick={onToggle} className="flex min-w-0 flex-1 items-center gap-3 text-left" aria-expanded={open}>
          {icon ? <span className="flex size-7 shrink-0 items-center justify-center rounded-md bg-zinc-100 text-zinc-600 [&>svg]:size-4">{icon}</span> : null}
          <span className="min-w-0 flex-1">
            <span className="flex items-center gap-2 text-sm font-semibold text-zinc-900">{title}</span>
            {!open && summary ? <span className="block truncate text-xs text-zinc-500">{summary}</span> : null}
          </span>
          <ChevronDown className={cx("size-4 shrink-0 text-zinc-400 transition-transform", open && "rotate-180")} />
        </button>
        {open && actions ? <div className="shrink-0">{actions}</div> : null}
      </header>
      {open ? <div className="space-y-4 border-t border-zinc-100 px-4 py-4">{children}</div> : null}
    </section>
  );
}

/** A small heading that groups controls inside a card. */
export function SubHeading({ children, action }: { children: ReactNode; action?: ReactNode }) {
  return (
    <div className="flex items-center justify-between gap-2 pt-1">
      <h3 className="text-[11px] font-semibold uppercase tracking-wide text-zinc-500">{children}</h3>
      {action}
    </div>
  );
}
