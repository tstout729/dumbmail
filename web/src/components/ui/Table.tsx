import type { ReactNode, TdHTMLAttributes, ThHTMLAttributes } from "react";
import { useNavigate } from "react-router";
import { Skeleton } from "./Layout";
import { cx } from "./cx";

export function Table({ children, className }: { children: ReactNode; className?: string }) {
  return (
    <div className={cx("overflow-x-auto rounded-lg border border-zinc-200 bg-white shadow-xs", className)}>
      <table className="w-full min-w-[640px] border-collapse text-sm">{children}</table>
    </div>
  );
}

export function Th({ className, children, ...rest }: ThHTMLAttributes<HTMLTableCellElement>) {
  return (
    <th
      className={cx(
        "border-b border-zinc-200 bg-zinc-50/70 px-4 py-2 text-left text-[11px] font-semibold uppercase tracking-wide text-zinc-500 whitespace-nowrap",
        className,
      )}
      {...rest}
    >
      {children}
    </th>
  );
}

export function Td({ className, children, ...rest }: TdHTMLAttributes<HTMLTableCellElement>) {
  return (
    <td className={cx("border-b border-zinc-100 px-4 py-2.5 align-middle", className)} {...rest}>
      {children}
    </td>
  );
}

/** A clickable row that navigates on click or Enter. Inner buttons should stopPropagation. */
export function RowLink({ to, children }: { to: string; children: ReactNode }) {
  const navigate = useNavigate();
  return (
    <tr
      tabIndex={0}
      onClick={(e) => (e.metaKey || e.ctrlKey ? window.open(to, "_blank") : navigate(to))}
      onKeyDown={(e) => {
        if (e.key === "Enter") navigate(to);
      }}
      className="cursor-pointer transition-colors hover:bg-zinc-50 focus:bg-zinc-50 focus:outline-none [&:last-child>td]:border-b-0"
    >
      {children}
    </tr>
  );
}

export function SkeletonRows({ cols, rows = 5 }: { cols: number; rows?: number }) {
  return (
    <>
      {Array.from({ length: rows }, (_, r) => (
        <tr key={r}>
          {Array.from({ length: cols }, (_, c) => (
            <Td key={c}>
              <Skeleton className={cx("h-4", c === 0 ? "w-40" : "w-16")} />
            </Td>
          ))}
        </tr>
      ))}
    </>
  );
}

export function EmptyRow({ cols, children }: { cols: number; children: ReactNode }) {
  return (
    <tr>
      <td colSpan={cols}>{children}</td>
    </tr>
  );
}
