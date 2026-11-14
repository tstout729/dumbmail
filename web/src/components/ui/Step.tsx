import type { ReactNode } from "react";
import { Check, ChevronDown } from "lucide-react";
import { cx } from "./cx";

type StepProps = {
  index: number;
  title: string;
  summary?: ReactNode;
  done?: boolean;
  open: boolean;
  onToggle: () => void;
  children: ReactNode;
  action?: ReactNode;
};

/** Klaviyo-style collapsible step card. */
export function Step({ index, title, summary, done, open, onToggle, children, action }: StepProps) {
  return (
    <section className={cx("rounded-lg border bg-white shadow-xs", open ? "border-zinc-300" : "border-zinc-200")}>
      <header className="flex items-center gap-3 px-4 py-3">
        <button type="button" onClick={onToggle} className="flex min-w-0 flex-1 items-center gap-3 text-left" aria-expanded={open}>
          <span
            className={cx(
              "flex size-6 shrink-0 items-center justify-center rounded-full text-xs font-semibold",
              done ? "bg-accent-600 text-white" : "bg-zinc-100 text-zinc-600",
            )}
          >
            {done ? <Check className="size-3.5" /> : index}
          </span>
          <span className="min-w-0 flex-1">
            <span className="block text-sm font-semibold text-zinc-900">{title}</span>
            {!open && summary ? <span className="block truncate text-xs text-zinc-500">{summary}</span> : null}
          </span>
          <ChevronDown className={cx("size-4 shrink-0 text-zinc-400 transition-transform", open && "rotate-180")} />
        </button>
        {action}
      </header>
      {open ? <div className="border-t border-zinc-100 px-4 py-4">{children}</div> : null}
    </section>
  );
}
