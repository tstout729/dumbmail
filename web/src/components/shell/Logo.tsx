export function Logo({ className }: { className?: string }) {
  return (
    <span className={className}>
      <span className="flex items-center gap-2 font-semibold tracking-tight text-zinc-950">
        <svg viewBox="0 0 32 32" className="size-6" aria-hidden>
          <rect width="32" height="32" rx="8" fill="#115f42" />
          <rect x="5.5" y="8.5" width="21" height="15" rx="2.5" fill="#fff" />
          <path d="M6.6 9.8 16 16.2l9.4-6.4" fill="none" stroke="#115f42" strokeWidth="1.7" strokeLinecap="round" strokeLinejoin="round" />
          <circle cx="12" cy="19.6" r="2.3" fill="#115f42" />
          <circle cx="20.2" cy="19.6" r="2.3" fill="#115f42" />
          <circle cx="12.9" cy="20.3" r="0.9" fill="#fff" />
          <circle cx="19.4" cy="18.8" r="0.9" fill="#fff" />
        </svg>
        DumbMail
      </span>
    </span>
  );
}
