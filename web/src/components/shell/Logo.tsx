export function Logo({ className }: { className?: string }) {
  return (
    <span className={className}>
      <span className="flex items-center gap-2 font-semibold tracking-tight text-zinc-950">
        <svg viewBox="0 0 32 32" className="size-6" aria-hidden>
          <rect width="32" height="32" rx="8" fill="#115f42" />
          <path d="M8 21c4-1 7-4 9-9l7-3-3 7c-5 2-8 5-9 9z" fill="#fff" />
        </svg>
        Postlark
      </span>
    </span>
  );
}
