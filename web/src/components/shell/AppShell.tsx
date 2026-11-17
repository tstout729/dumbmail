import { Suspense, useEffect, useState } from "react";
import { Navigate, Outlet, useLocation, useNavigate } from "react-router";
import { useMutationState, useQueryClient } from "@tanstack/react-query";
import { Menu as MenuIcon, Search } from "lucide-react";
import { ApiError, authMutationKey, keys, onUnauthorized, useMe } from "../../api";
import { isTypingTarget, useWindowKeydown } from "../../lib/hooks";
import { Button, Skeleton } from "../ui";
import { CommandPalette } from "./CommandPalette";
import { Logo } from "./Logo";
import { SendingPausedBanner } from "./SendingPausedBanner";
import { Sidebar } from "./Sidebar";

function Splash() {
  return (
    <div className="flex min-h-dvh items-center justify-center">
      <div className="flex flex-col items-center gap-3">
        <Logo />
        <Skeleton className="h-1 w-24" />
      </div>
    </div>
  );
}

/** Gate for signed-in routes. 401 → /login; any later 401 from the API also lands there. */
export function RequireAuth() {
  const me = useMe();
  const location = useLocation();
  const navigate = useNavigate();
  const qc = useQueryClient();

  // External system: the API client's 401 hook (module-level callback); unregistered on unmount.
  useEffect(
    () =>
      onUnauthorized(() => {
        qc.removeQueries({ queryKey: keys.me });
        navigate("/login", { replace: true });
      }),
    [qc, navigate],
  );

  if (me.isPending) return <Splash />;
  if (me.error) {
    if (me.error instanceof ApiError && me.error.status === 401) {
      const next = location.pathname + location.search;
      return <Navigate to={next === "/" ? "/login" : `/login?next=${encodeURIComponent(next)}`} replace />;
    }
    return (
      <div className="flex min-h-dvh flex-col items-center justify-center gap-3 p-6 text-center">
        <Logo />
        <p className="text-sm text-zinc-600">{me.error.message}</p>
        <Button onClick={() => void me.refetch()}>Try again</Button>
      </div>
    );
  }
  return <Outlet />;
}

/** Public pages (login/signup) bounce signed-in users to Home. */
export function RedirectIfAuthed() {
  const me = useMe();
  // After a successful login/signup the page navigates on its own (to /onboarding or ?next=).
  const justAuthed = useMutationState({ filters: { mutationKey: authMutationKey, status: "success" } }).length > 0;
  if (me.isPending) return <Splash />;
  if (me.data && !justAuthed) return <Navigate to="/" replace />;
  return <Outlet />;
}

function ContentFallback() {
  return (
    <div className="mx-auto max-w-6xl space-y-3 px-4 py-6 sm:px-6 lg:px-8">
      <Skeleton className="h-7 w-48" />
      <Skeleton className="h-4 w-72" />
      <Skeleton className="mt-6 h-64 w-full" />
    </div>
  );
}

export function AppShell() {
  const [sidebarOpen, setSidebarOpen] = useState(false);
  const [paletteOpen, setPaletteOpen] = useState(false);

  useWindowKeydown((e) => {
    if ((e.metaKey || e.ctrlKey) && e.key.toLowerCase() === "k") {
      e.preventDefault();
      setPaletteOpen((o) => !o);
    } else if (e.key === "/" && !isTypingTarget(e.target) && !paletteOpen) {
      e.preventDefault();
      setPaletteOpen(true);
    }
  });

  return (
    <div className="min-h-dvh">
      <Sidebar open={sidebarOpen} onClose={() => setSidebarOpen(false)} onOpenPalette={() => setPaletteOpen(true)} />
      <div className="lg:pl-60">
        <header className="sticky top-0 z-20 flex h-12 items-center gap-2 border-b border-zinc-200 bg-white/90 px-3 backdrop-blur lg:hidden">
          <button type="button" className="rounded-md p-1.5 text-zinc-600 hover:bg-zinc-100" onClick={() => setSidebarOpen(true)} aria-label="Open menu">
            <MenuIcon className="size-5" />
          </button>
          <Logo className="flex-1" />
          <button type="button" className="rounded-md p-1.5 text-zinc-600 hover:bg-zinc-100" onClick={() => setPaletteOpen(true)} aria-label="Search">
            <Search className="size-5" />
          </button>
        </header>
        <SendingPausedBanner />
        <main>
          <Suspense fallback={<ContentFallback />}>
            <Outlet />
          </Suspense>
        </main>
      </div>
      {paletteOpen ? <CommandPalette onClose={() => setPaletteOpen(false)} /> : null}
    </div>
  );
}
