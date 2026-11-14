import { Copy } from "lucide-react";
import { copyText } from "../../lib/clipboard";
import { Button } from "./Button";
import { cx } from "./cx";

export function CodeBlock({ code, label, className }: { code: string; label?: string; className?: string }) {
  return (
    <div className={cx("overflow-hidden rounded-lg border border-zinc-200 bg-zinc-950", className)}>
      <div className="flex items-center justify-between border-b border-white/10 px-3 py-1.5">
        <span className="text-[11px] font-medium text-zinc-400">{label ?? ""}</span>
        <button
          type="button"
          onClick={() => void copyText(code)}
          className="inline-flex items-center gap-1 rounded px-1.5 py-0.5 text-[11px] font-medium text-zinc-300 hover:bg-white/10 hover:text-white"
        >
          <Copy className="size-3" /> Copy
        </button>
      </div>
      <pre className="overflow-x-auto p-3 font-mono text-xs leading-relaxed text-zinc-100">
        <code>{code}</code>
      </pre>
    </div>
  );
}

export function CopyButton({ text, label = "Copy" }: { text: string; label?: string }) {
  return (
    <Button size="sm" variant="secondary" icon={<Copy className="size-3" />} onClick={() => void copyText(text)}>
      {label}
    </Button>
  );
}

export function CopyField({ value }: { value: string }) {
  return (
    <div className="flex items-center gap-1.5">
      <code className="min-w-0 flex-1 truncate rounded-md border border-zinc-200 bg-zinc-50 px-2 py-1.5 font-mono text-xs text-zinc-800">
        {value}
      </code>
      <button
        type="button"
        onClick={() => void copyText(value)}
        className="rounded-md p-1.5 text-zinc-500 hover:bg-zinc-100 hover:text-zinc-900"
        aria-label="Copy"
        title="Copy"
      >
        <Copy className="size-3.5" />
      </button>
    </div>
  );
}
