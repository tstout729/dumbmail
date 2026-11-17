import type { ReactNode } from "react";
import { useSiteHref } from "../../api";
import { Logo } from "../../components/shell/Logo";

export function AuthLayout({ title, subtitle, children, footer }: { title: string; subtitle?: string; children: ReactNode; footer: ReactNode }) {
  const about = useSiteHref("/about");
  const privacy = useSiteHref("/privacy");
  const terms = useSiteHref("/terms");
  return (
    <div className="flex min-h-dvh flex-col items-center justify-center bg-zinc-50 px-4 py-10">
      <Logo className="mb-8" />
      <div className="w-full max-w-sm rounded-xl border border-zinc-200 bg-white p-6 shadow-sm">
        <h1 className="text-lg font-semibold tracking-tight text-zinc-950">{title}</h1>
        {subtitle ? <p className="mt-1 text-sm text-zinc-500">{subtitle}</p> : null}
        <div className="mt-5">{children}</div>
      </div>
      <div className="mt-5 text-sm text-zinc-500">{footer}</div>
      {/* Server-rendered pages, so plain links (full page loads), not router links. */}
      <nav className="mt-8 flex gap-4 text-xs text-zinc-400" aria-label="About DumbMail">
        <a href={about} className="hover:text-zinc-600 hover:underline">
          About
        </a>
        <a href={privacy} className="hover:text-zinc-600 hover:underline">
          Privacy
        </a>
        <a href={terms} className="hover:text-zinc-600 hover:underline">
          Terms
        </a>
      </nav>
    </div>
  );
}
