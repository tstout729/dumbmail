import type { InputHTMLAttributes } from "react";

type InlineTitleProps = Omit<InputHTMLAttributes<HTMLInputElement>, "value" | "className"> & {
  value: string;
};

/** A page title that edits in place and grows with its text. */
export function InlineTitle({ value, placeholder, ...rest }: InlineTitleProps) {
  return (
    <span className="-mx-1 inline-grid min-w-0 max-w-full font-semibold">
      {/* Invisible copy of the text sizes the input to fit it exactly. */}
      <span aria-hidden="true" className="invisible col-start-1 row-start-1 overflow-hidden whitespace-pre px-1 pr-2">
        {value || placeholder || " "}
      </span>
      <input
        size={1}
        {...rest}
        value={value}
        placeholder={placeholder}
        className="col-start-1 row-start-1 w-full min-w-0 rounded-md bg-transparent px-1 font-semibold outline-none placeholder:text-zinc-400 hover:bg-zinc-100 focus:bg-white focus:ring-2 focus:ring-accent-600/20 disabled:hover:bg-transparent"
      />
    </span>
  );
}
