import { createContext, useContext, useId, useState, type ComponentProps, type InputHTMLAttributes, type ReactNode } from "react";
import { cx } from "./cx";
import { X } from "lucide-react";

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

type ColorInputProps = {
  value: string;
  onChange: (value: string) => void;
  /** The color used while `value` is empty, shown in the swatch and as the text placeholder. */
  placeholder?: string;
  "aria-label"?: string;
};

const hex6 = /^#[0-9a-f]{6}$/i;

export function ColorInput({ value, onChange, placeholder, "aria-label": ariaLabel }: ColorInputProps) {
  const swatch = hex6.test(value) ? value : placeholder && hex6.test(placeholder) ? placeholder : "#115f42";
  return (
    <div className="flex items-center gap-2">
      <input
        type="color"
        value={swatch}
        onChange={(e) => onChange(e.target.value)}
        className="h-8.5 w-10 shrink-0 cursor-pointer rounded-md border border-zinc-300 bg-white p-1"
        aria-label={ariaLabel ? `${ariaLabel}: pick color` : "Pick color"}
      />
      <Input value={value} placeholder={placeholder} onChange={(e) => onChange(e.target.value)} className="font-mono w-28" aria-label={ariaLabel} />
    </div>
  );
}

type RangeInputProps = {
  value: number | undefined;
  /** Shown (and used by the slider) while `value` is unset. */
  placeholder: number;
  min: number;
  max: number;
  step?: number;
  unit?: string;
  onChange: (value: number) => void;
  "aria-label"?: string;
};

/** A slider with a number box beside it. The number box clamps on blur so typing "1" on the way to "16" isn't fought. */
export function RangeInput({ value, placeholder, min, max, step = 1, unit, onChange, "aria-label": ariaLabel }: RangeInputProps) {
  const fieldId = useContext(FieldIdContext);
  const [text, setText] = useState<string | null>(null);
  const shown = value ?? placeholder;
  const clamp = (n: number) => Math.min(max, Math.max(min, Math.round(n / step) * step));
  return (
    <div className="flex items-center gap-3">
      <input
        type="range"
        min={min}
        max={max}
        step={step}
        value={shown}
        onChange={(e) => onChange(Number(e.target.value))}
        className="h-1.5 min-w-0 flex-1 cursor-pointer accent-accent-700"
        aria-label={ariaLabel}
      />
      <div className="flex shrink-0 items-center gap-1">
        <input
          id={fieldId}
          type="number"
          min={min}
          max={max}
          step={step}
          value={text ?? (value === undefined ? "" : String(value))}
          placeholder={String(placeholder)}
          onChange={(e) => {
            setText(e.target.value);
            const n = Number(e.target.value);
            if (e.target.value !== "" && Number.isFinite(n) && n >= min && n <= max) onChange(n);
          }}
          onBlur={() => {
            if (text !== null && text !== "" && Number.isFinite(Number(text))) onChange(clamp(Number(text)));
            setText(null);
          }}
          className={cx(control, "h-8 w-16 px-2 text-right tabular-nums")}
          aria-label={ariaLabel}
        />
        {unit ? <span className="w-5 text-xs text-zinc-500">{unit}</span> : null}
      </div>
    </div>
  );
}

type TagInputProps = {
  value: string[];
  onChange: (value: string[]) => void;
  placeholder?: string;
  "aria-label"?: string;
};

/** Free-text chips: Enter, comma or blur adds the typed value; Backspace on an empty input removes the last one. */
export function TagInput({ value, onChange, placeholder, "aria-label": ariaLabel }: TagInputProps) {
  const fieldId = useContext(FieldIdContext);
  const [text, setText] = useState("");
  const commit = () => {
    const tag = text.trim().replace(/,$/, "").trim();
    setText("");
    if (tag && !value.includes(tag)) onChange([...value, tag]);
  };
  return (
    <div
      className={cx(
        control,
        "flex min-h-8.5 w-full flex-wrap items-center gap-1 py-1 focus-within:border-accent-600 focus-within:ring-2 focus-within:ring-accent-600/20",
      )}
    >
      {value.map((tag) => (
        <span key={tag} className="inline-flex items-center gap-0.5 rounded bg-zinc-100 py-0.5 pl-1.5 pr-0.5 font-mono text-xs text-zinc-800">
          {tag}
          <button
            type="button"
            onClick={() => onChange(value.filter((t) => t !== tag))}
            className="rounded p-0.5 text-zinc-400 hover:bg-zinc-200 hover:text-zinc-800"
            aria-label={`Remove ${tag}`}
          >
            <X className="size-3" />
          </button>
        </span>
      ))}
      <input
        id={fieldId}
        aria-label={ariaLabel}
        value={text}
        onChange={(e) => setText(e.target.value)}
        onKeyDown={(e) => {
          if (e.key === "Enter" || e.key === ",") {
            e.preventDefault();
            commit();
          } else if (e.key === "Backspace" && text === "" && value.length > 0) {
            onChange(value.slice(0, -1));
          }
        }}
        onBlur={commit}
        placeholder={value.length === 0 ? placeholder : undefined}
        className="h-6 min-w-24 flex-1 bg-transparent text-sm outline-none placeholder:text-zinc-400"
      />
    </div>
  );
}
