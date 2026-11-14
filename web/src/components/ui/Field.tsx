import { createContext, useContext, useId, type ComponentProps, type InputHTMLAttributes, type ReactNode } from "react";
import { cx } from "./cx";

const control =
  "block rounded-md border border-zinc-300 bg-white px-2.5 text-sm text-zinc-900 placeholder:text-zinc-400 shadow-xs transition-colors hover:border-zinc-400 focus:border-accent-600 focus:outline-none focus:ring-2 focus:ring-accent-600/20 disabled:bg-zinc-50 disabled:text-zinc-500";

/** Full width unless the caller sets its own width (Tailwind can't resolve two conflicting width classes by order). */
function width(className: string | undefined) {
  return className && /(^|\s)(w-|min-w-|max-w-|flex-1)/.test(className) ? className : cx("w-full", className);
}

/** A Field hands its label's id to the first control inside it, so the label is announced and clickable. */
const FieldIdContext = createContext<string | undefined>(undefined);

export function Input({ className, id, ...rest }: ComponentProps<"input">) {
  const fieldId = useContext(FieldIdContext);
  return <input id={id ?? fieldId} className={cx(control, "h-8.5", width(className))} {...rest} />;
}

export function Textarea({ className, id, ...rest }: ComponentProps<"textarea">) {
  const fieldId = useContext(FieldIdContext);
  return <textarea id={id ?? fieldId} className={cx(control, "py-2 leading-relaxed", width(className))} {...rest} />;
}

export function Select({ className, children, id, ...rest }: ComponentProps<"select">) {
  const fieldId = useContext(FieldIdContext);
  return (
    <select id={id ?? fieldId} className={cx(control, "h-8.5 pr-8 appearance-auto", width(className))} {...rest}>
      {children}
    </select>
  );
}

type FieldProps = {
  label: ReactNode;
  hint?: ReactNode;
  error?: string | null;
  children: ReactNode;
  className?: string;
  htmlFor?: string;
};

export function Field({ label, hint, error, children, className, htmlFor }: FieldProps) {
  const generated = useId();
  const id = htmlFor ?? generated;
  return (
    <div className={cx("space-y-1", className)}>
      <label htmlFor={id} className="block text-xs font-medium text-zinc-700">
        {label}
      </label>
      <FieldIdContext value={id}>{children}</FieldIdContext>
      {error ? (
        <p className="text-xs text-red-600">{error}</p>
      ) : hint ? (
        <p className="text-xs text-zinc-500">{hint}</p>
      ) : null}
    </div>
  );
}

type CheckboxProps = Omit<InputHTMLAttributes<HTMLInputElement>, "type"> & { label: ReactNode; description?: ReactNode };

export function Checkbox({ label, description, className, ...rest }: CheckboxProps) {
  return (
    <label className={cx("flex items-start gap-2 cursor-pointer select-none", className)}>
      <input type="checkbox" className="mt-0.5 size-4 rounded border-zinc-300 accent-accent-700" {...rest} />
      <span>
        <span className="text-sm text-zinc-800">{label}</span>
        {description ? <span className="block text-xs text-zinc-500">{description}</span> : null}
      </span>
    </label>
  );
}

type ToggleProps = { checked: boolean; onChange: (checked: boolean) => void; label?: string; disabled?: boolean };

export function Toggle({ checked, onChange, label, disabled }: ToggleProps) {
  return (
    <button
      type="button"
      role="switch"
      aria-checked={checked}
      aria-label={label}
      disabled={disabled}
      onClick={() => onChange(!checked)}
      className={cx(
        "relative inline-flex h-5 w-9 shrink-0 items-center rounded-full transition-colors disabled:opacity-50",
        checked ? "bg-accent-600" : "bg-zinc-300",
      )}
    >
      <span
        className={cx(
          "inline-block size-4 rounded-full bg-white shadow transition-transform",
          checked ? "translate-x-4.5" : "translate-x-0.5",
        )}
      />
    </button>
  );
}

type SegmentedProps<T extends string> = {
  value: T;
  onChange: (value: T) => void;
  options: { value: T; label: ReactNode }[];
  size?: "sm" | "md";
};

export function Segmented<T extends string>({ value, onChange, options, size = "md" }: SegmentedProps<T>) {
  return (
    <div className="inline-flex rounded-md bg-zinc-100 p-0.5" role="radiogroup">
      {options.map((o) => (
        <button
          key={o.value}
          type="button"
          role="radio"
          aria-checked={value === o.value}
          onClick={() => onChange(o.value)}
          className={cx(
            "inline-flex items-center gap-1 rounded px-2.5 font-medium transition-colors",
            size === "sm" ? "h-6 text-xs" : "h-7 text-xs",
            value === o.value ? "bg-white text-zinc-900 shadow-xs" : "text-zinc-600 hover:text-zinc-900",
          )}
        >
          {o.label}
        </button>
      ))}
    </div>
  );
}

export function ColorInput({ value, onChange }: { value: string; onChange: (value: string) => void }) {
  return (
    <div className="flex items-center gap-2">
      <input
        type="color"
        value={/^#[0-9a-f]{6}$/i.test(value) ? value : "#115f42"}
        onChange={(e) => onChange(e.target.value)}
        className="h-8.5 w-10 cursor-pointer rounded-md border border-zinc-300 bg-white p-1"
        aria-label="Pick color"
      />
      <Input value={value} onChange={(e) => onChange(e.target.value)} className="font-mono w-28" />
    </div>
  );
}
