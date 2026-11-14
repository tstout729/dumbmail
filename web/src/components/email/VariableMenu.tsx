import { Braces } from "lucide-react";
import { Menu, buttonClass } from "../ui";
import { templateVariables } from "./blocks";

export function VariableMenu({ onInsert, compact }: { onInsert: (text: string) => void; compact?: boolean }) {
  return (
    <Menu
      trigger={(props) => (
        <button type="button" {...props} className={buttonClass("ghost", "sm", "text-zinc-600")} title="Insert variable">
          <Braces className="size-3.5" />
          {compact ? null : "Variable"}
        </button>
      )}
      items={templateVariables.map((v) => ({ label: v.label, onSelect: () => onInsert(v.value) }))}
    />
  );
}

/** Insert text at the cursor of an input/textarea and return the new value. */
export function insertAtCursor(el: HTMLInputElement | HTMLTextAreaElement | null, current: string, text: string): string {
  if (!el) return current + text;
  const start = el.selectionStart ?? current.length;
  const end = el.selectionEnd ?? current.length;
  const next = current.slice(0, start) + text + current.slice(end);
  requestAnimationFrame(() => {
    el.focus();
    el.setSelectionRange(start + text.length, start + text.length);
  });
  return next;
}

/** Wrap the current selection in a textarea with before/after strings. */
export function wrapSelection(el: HTMLTextAreaElement | null, current: string, before: string, after: string): string {
  if (!el) return current + before + after;
  const start = el.selectionStart;
  const end = el.selectionEnd;
  const next = current.slice(0, start) + before + current.slice(start, end) + after + current.slice(end);
  requestAnimationFrame(() => {
    el.focus();
    el.setSelectionRange(start + before.length, end + before.length);
  });
  return next;
}
