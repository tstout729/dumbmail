import { CheckCircle2, Info, X, XCircle } from "lucide-react";
import { dismissToast, useToasts } from "../../lib/toast";
import { cx } from "./cx";

export function Toaster() {
  const toasts = useToasts();
  return (
    <div className="pointer-events-none fixed inset-x-0 bottom-4 z-[60] flex flex-col items-center gap-2 px-4 sm:items-end sm:pr-6" aria-live="polite">
      {toasts.map((t) => (
        <div
          key={t.id}
          className={cx(
            "pl-pop pointer-events-auto flex w-full max-w-sm items-start gap-2.5 rounded-lg px-3.5 py-2.5 text-sm shadow-lg ring-1",
            t.kind === "error" ? "bg-white text-red-800 ring-red-200" : "bg-zinc-900 text-white ring-zinc-900",
          )}
        >
          {t.kind === "error" ? (
            <XCircle className="mt-0.5 size-4 shrink-0 text-red-600" />
          ) : t.kind === "success" ? (
            <CheckCircle2 className="mt-0.5 size-4 shrink-0 text-accent-300" />
          ) : (
            <Info className="mt-0.5 size-4 shrink-0 text-zinc-300" />
          )}
          <span className="flex-1">{t.message}</span>
          <button type="button" aria-label="Dismiss" onClick={() => dismissToast(t.id)} className="opacity-60 hover:opacity-100">
            <X className="size-3.5" />
          </button>
        </div>
      ))}
    </div>
  );
}
