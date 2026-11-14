import { useEffect, useRef, useState, type ReactNode } from "react";
import { createPortal } from "react-dom";
import { X } from "lucide-react";
import { Button, IconButton } from "./Button";
import { cx } from "./cx";
import { useWindowKeydown } from "../../lib/hooks";

function useBodyScrollLock() {
  // External system: document.body style; restores the previous overflow on close.
  useEffect(() => {
    const previous = document.body.style.overflow;
    document.body.style.overflow = "hidden";
    return () => {
      document.body.style.overflow = previous;
    };
  }, []);
}

type ModalProps = {
  title: ReactNode;
  onClose: () => void;
  children: ReactNode;
  footer?: ReactNode;
  size?: "sm" | "md" | "lg" | "xl" | "full";
  description?: ReactNode;
};

const modalSizes = {
  sm: "max-w-sm",
  md: "max-w-lg",
  lg: "max-w-2xl",
  xl: "max-w-5xl",
  full: "max-w-[min(1400px,calc(100vw-2rem))] h-[calc(100dvh-2rem)]",
};

/** Mount it only while open: `{open && <Modal .../>}`. */
export function Modal({ title, onClose, children, footer, size = "md", description }: ModalProps) {
  useBodyScrollLock();
  useWindowKeydown((e) => {
    if (e.key === "Escape") onClose();
  });
  return createPortal(
    <div className="fixed inset-0 z-50 flex items-start justify-center overflow-y-auto bg-zinc-950/40 p-4 sm:items-center">
      <div className="absolute inset-0" onClick={onClose} aria-hidden />
      <div
        role="dialog"
        aria-modal="true"
        className={cx(
          "pl-pop relative flex w-full flex-col rounded-xl bg-white shadow-2xl ring-1 ring-zinc-950/5",
          modalSizes[size],
        )}
      >
        <header className="flex items-start justify-between gap-4 border-b border-zinc-100 px-5 py-3.5">
          <div>
            <h2 className="text-base font-semibold text-zinc-950">{title}</h2>
            {description ? <p className="mt-0.5 text-sm text-zinc-500">{description}</p> : null}
          </div>
          <IconButton label="Close" onClick={onClose} className="-mr-2">
            <X className="size-4" />
          </IconButton>
        </header>
        <div className={cx("px-5 py-4", size === "full" && "min-h-0 flex-1 overflow-hidden p-0")}>{children}</div>
        {footer ? (
          <footer className="flex flex-wrap items-center justify-end gap-2 border-t border-zinc-100 px-5 py-3">
            {footer}
          </footer>
        ) : null}
      </div>
    </div>,
    document.body,
  );
}

type DrawerProps = { title: ReactNode; onClose: () => void; children: ReactNode; footer?: ReactNode; width?: string };

/** Right-side panel. Mount it only while open. */
export function Drawer({ title, onClose, children, footer, width = "w-[min(440px,100vw)]" }: DrawerProps) {
  useWindowKeydown((e) => {
    if (e.key === "Escape" && !document.querySelector("[role=dialog][aria-modal=true]")) onClose();
  });
  return createPortal(
    <div className="fixed inset-0 z-40 flex justify-end">
      <div className="absolute inset-0 bg-zinc-950/20 lg:bg-transparent" onClick={onClose} aria-hidden />
      <aside
        className={cx(
          "pl-slide relative flex h-full flex-col border-l border-zinc-200 bg-white shadow-xl",
          width,
        )}
      >
        <header className="flex items-center justify-between gap-2 border-b border-zinc-100 px-4 py-3">
          <h2 className="text-sm font-semibold text-zinc-950">{title}</h2>
          <IconButton label="Close panel" onClick={onClose}>
            <X className="size-4" />
          </IconButton>
        </header>
        <div className="min-h-0 flex-1 overflow-y-auto p-4">{children}</div>
        {footer ? <footer className="flex items-center gap-2 border-t border-zinc-100 px-4 py-3">{footer}</footer> : null}
      </aside>
    </div>,
    document.body,
  );
}

type ConfirmProps = {
  title: string;
  body?: ReactNode;
  confirmLabel?: string;
  danger?: boolean;
  loading?: boolean;
  onConfirm: () => void;
  onClose: () => void;
};

export function ConfirmDialog({ title, body, confirmLabel = "Confirm", danger, loading, onConfirm, onClose }: ConfirmProps) {
  return (
    <Modal
      title={title}
      onClose={onClose}
      size="sm"
      footer={
        <>
          <Button variant="ghost" onClick={onClose}>
            Cancel
          </Button>
          <Button variant={danger ? "danger" : "primary"} loading={loading} onClick={onConfirm} autoFocus>
            {confirmLabel}
          </Button>
        </>
      }
    >
      {body ? <div className="text-sm text-zinc-600">{body}</div> : null}
    </Modal>
  );
}

type MenuItem = { label: string; icon?: ReactNode; onSelect: () => void; danger?: boolean; disabled?: boolean };

type MenuProps = {
  trigger: (props: { onClick: () => void; "aria-expanded": boolean }) => ReactNode;
  items: MenuItem[];
  align?: "left" | "right";
  up?: boolean;
  /** Wrapper classes; defaults to inline-block so the trigger keeps its own width. */
  className?: string;
};

export function Menu({ trigger, items, align = "right", up, className = "relative inline-block" }: MenuProps) {
  const [open, setOpen] = useState(false);
  const ref = useRef<HTMLDivElement>(null);
  // External system: document pointer events, to close the menu on outside click.
  useEffect(() => {
    if (!open) return;
    const onDown = (e: PointerEvent) => {
      if (ref.current && !ref.current.contains(e.target as Node)) setOpen(false);
    };
    document.addEventListener("pointerdown", onDown);
    return () => document.removeEventListener("pointerdown", onDown);
  }, [open]);
  return (
    <div
      className={className}
      ref={ref}
      onClick={(e) => e.stopPropagation()}
      onKeyDown={(e) => {
        e.stopPropagation();
        if (e.key === "Escape") setOpen(false);
      }}
    >
      {trigger({ onClick: () => setOpen((o) => !o), "aria-expanded": open })}
      {open ? (
        <div
          role="menu"
          className={cx(
            "pl-pop absolute z-30 min-w-44 rounded-lg border border-zinc-200 bg-white p-1 shadow-lg",
            align === "right" ? "right-0" : "left-0",
            up ? "bottom-full mb-1" : "top-full mt-1",
          )}
        >
          {items.map((item) => (
            <button
              key={item.label}
              role="menuitem"
              type="button"
              disabled={item.disabled}
              onClick={() => {
                setOpen(false);
                item.onSelect();
              }}
              className={cx(
                "flex w-full items-center gap-2 rounded-md px-2.5 py-1.5 text-left text-sm disabled:opacity-40",
                item.danger ? "text-red-700 hover:bg-red-50" : "text-zinc-700 hover:bg-zinc-100",
              )}
            >
              {item.icon}
              {item.label}
            </button>
          ))}
        </div>
      ) : null}
    </div>
  );
}
