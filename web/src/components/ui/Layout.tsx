import type { ReactNode } from "react";
import { Link } from "react-router";
import { ChevronRight } from "lucide-react";
import { cx } from "./cx";

type PageHeaderProps = {
  title: ReactNode;
  description?: ReactNode;
  actions?: ReactNode;
  breadcrumbs?: { label: string; to: string }[];
};

export function PageHeader({ title, description, actions, breadcrumbs }: PageHeaderProps) {
  return (
    <div className="mb-5">
      {breadcrumbs && breadcrumbs.length > 0 ? (
        <nav className="mb-1.5 flex items-center gap-1 text-xs text-zinc-500">
          {breadcrumbs.map((b) => (
            <span key={b.to} className="flex items-center gap-1">
              <Link to={b.to} className="hover:text-zinc-900">
                {b.label}
              </Link>
              <ChevronRight className="size-3" />
            </span>
          ))}
        </nav>
      ) : null}
      <div className="flex flex-wrap items-start justify-between gap-3">
        <div className="min-w-0">
          <h1 className="text-xl font-semibold tracking-tight text-zinc-950 truncate">{title}</h1>
          {description ? <p className="mt-0.5 text-sm text-zinc-500">{description}</p> : null}
        </div>
        {actions ? <div className="flex flex-wrap items-center gap-2">{actions}</div> : null}
      </div>
    </div>
  );
}

export function Page({ children, wide }: { children: ReactNode; wide?: boolean }) {
  return <div className={cx("mx-auto w-full px-4 py-6 sm:px-6 lg:px-8", wide ? "max-w-[1400px]" : "max-w-6xl")}>{children}</div>;
}

type CardProps = { children: ReactNode; className?: string; title?: ReactNode; actions?: ReactNode; padded?: boolean };

export function Card({ children, className, title, actions, padded = true }: CardProps) {
  return (
    <section className={cx("rounded-lg border border-zinc-200 bg-white shadow-xs", className)}>
      {title || actions ? (
        <header className="flex items-center justify-between gap-2 border-b border-zinc-100 px-4 py-3">
          <h2 className="text-sm font-semibold text-zinc-900">{title}</h2>
          {actions}
        </header>
      ) : null}
      <div className={padded ? "p-4" : undefined}>{children}</div>
    </section>
  );
}

export function Skeleton({ className }: { className?: string }) {
  return <div className={cx("pl-skeleton rounded", className)} />;
}

type EmptyStateProps = { icon?: ReactNode; title: string; description?: ReactNode; action?: ReactNode };

export function EmptyState({ icon, title, description, action }: EmptyStateProps) {
  return (
    <div className="flex flex-col items-center justify-center px-6 py-14 text-center">
      {icon ? <div className="mb-3 rounded-full bg-zinc-100 p-3 text-zinc-500">{icon}</div> : null}
      <h3 className="text-sm font-semibold text-zinc-900">{title}</h3>
      {description ? <p className="mt-1 max-w-sm text-sm text-zinc-500">{description}</p> : null}
      {action ? <div className="mt-4">{action}</div> : null}
    </div>
  );
}

const tones = {
  gray: "bg-zinc-100 text-zinc-700 ring-zinc-200",
  green: "bg-accent-50 text-accent-800 ring-accent-200",
  blue: "bg-sky-50 text-sky-800 ring-sky-200",
  amber: "bg-amber-50 text-amber-800 ring-amber-200",
  red: "bg-red-50 text-red-700 ring-red-200",
  violet: "bg-violet-50 text-violet-800 ring-violet-200",
} as const;

export type Tone = keyof typeof tones;

export function Badge({ tone = "gray", children, dot }: { tone?: Tone; children: ReactNode; dot?: boolean }) {
  return (
    <span
      className={cx(
        "inline-flex items-center gap-1 rounded-full px-2 py-0.5 text-[11px] font-medium ring-1 ring-inset whitespace-nowrap",
        tones[tone],
      )}
    >
      {dot ? <span className="size-1.5 rounded-full bg-current opacity-70" /> : null}
      {children}
    </span>
  );
}

const statusTones: Record<string, Tone> = {
  draft: "gray",
  scheduled: "blue",
  sending: "amber",
  // A/B test slice sent; waiting on results before the winner goes out.
  testing: "violet",
  sent: "green",
  cancelled: "gray",
  failed: "red",
  live: "green",
  paused: "amber",
  verified: "green",
  pending: "amber",
  not_started: "gray",
  subscribed: "green",
  unsubscribed: "red",
  never_subscribed: "gray",
  // DNS records, domains and phone numbers (v0.2)
  missing: "red",
  recommended: "blue",
  active: "green",
  needs_verification: "amber",
  verification_pending: "blue",
  rejected: "red",
  released: "gray",
};

/** `label` overrides the text derived from the status key (e.g. "Needs verification"). */
export function StatusBadge({ status, label }: { status: string; label?: string }) {
  const text = label ?? status.replace(/_/g, " ");
  return (
    <Badge tone={statusTones[status] ?? "gray"} dot>
      {text.charAt(0).toUpperCase() + text.slice(1)}
    </Badge>
  );
}

type NoticeTone = "green" | "amber" | "red" | "blue" | "gray";

const noticeTones: Record<NoticeTone, string> = {
  green: "border-accent-200 bg-accent-50 text-accent-900",
  amber: "border-amber-200 bg-amber-50 text-amber-900",
  red: "border-red-200 bg-red-50 text-red-800",
  blue: "border-sky-200 bg-sky-50 text-sky-900",
  gray: "border-zinc-200 bg-zinc-50 text-zinc-800",
};

/** A full-width status line: icon, bold title and optional detail. */
export function Notice({ tone, icon, title, children }: { tone: NoticeTone; icon?: ReactNode; title: ReactNode; children?: ReactNode }) {
  return (
    <div className={cx("flex items-start gap-2.5 rounded-lg border px-3.5 py-2.5 text-sm", noticeTones[tone])} role="status">
      {icon ? <span className="mt-0.5 shrink-0 [&>svg]:size-4">{icon}</span> : null}
      <div className="min-w-0">
        <div className="font-medium">{title}</div>
        {children ? <div className="mt-0.5 opacity-90">{children}</div> : null}
      </div>
    </div>
  );
}

type TabsProps<T extends string> = {
  value: T;
  onChange: (value: T) => void;
  tabs: { value: T; label: ReactNode; count?: number }[];
};

export function Tabs<T extends string>({ value, onChange, tabs }: TabsProps<T>) {
  return (
    <div className="mb-4 flex gap-1 overflow-x-auto border-b border-zinc-200" role="tablist">
      {tabs.map((t) => (
        <button
          key={t.value}
          role="tab"
          type="button"
          aria-selected={value === t.value}
          onClick={() => onChange(t.value)}
          className={cx(
            "-mb-px flex items-center gap-1.5 whitespace-nowrap border-b-2 px-3 py-2 text-sm font-medium transition-colors",
            value === t.value
              ? "border-accent-700 text-zinc-950"
              : "border-transparent text-zinc-500 hover:text-zinc-900",
          )}
        >
          {t.label}
          {t.count !== undefined ? (
            <span className="rounded-full bg-zinc-100 px-1.5 text-[11px] text-zinc-600">{t.count}</span>
          ) : null}
        </button>
      ))}
    </div>
  );
}

type StatProps = { label: string; value: ReactNode; sub?: ReactNode; loading?: boolean };

export function Stat({ label, value, sub, loading }: StatProps) {
  return (
    <div className="rounded-lg border border-zinc-200 bg-white px-4 py-3 shadow-xs">
      <div className="text-xs font-medium text-zinc-500">{label}</div>
      {loading ? (
        <Skeleton className="mt-2 h-6 w-20" />
      ) : (
        <div className="mt-1 text-xl font-semibold tracking-tight tabular-nums text-zinc-950">{value}</div>
      )}
      {sub ? <div className="mt-0.5 text-xs text-zinc-500">{sub}</div> : null}
    </div>
  );
}

export function InlineError({ message }: { message: string | null | undefined }) {
  if (!message) return null;
  return <div className="rounded-md border border-red-200 bg-red-50 px-3 py-2 text-sm text-red-700">{message}</div>;
}

export function Kbd({ children }: { children: ReactNode }) {
  return (
    <kbd className="rounded border border-zinc-300 bg-zinc-50 px-1 font-sans text-[10px] font-medium text-zinc-500">
      {children}
    </kbd>
  );
}
