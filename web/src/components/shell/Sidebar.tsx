import { NavLink, useNavigate } from "react-router";
import { ChevronsUpDown, LogOut, Search, Settings, X } from "lucide-react";
import { useLogout, useSession } from "../../api";
import { Kbd, Menu, cx } from "../ui";
import { Logo } from "./Logo";
import { bottomNav, mainNav, type NavItem } from "./nav";

function NavEntry({ item, onNavigate }: { item: NavItem; onNavigate: () => void }) {
  const Icon = item.icon;
  return (
    <NavLink
      to={item.to}
      end={item.end}
      onClick={onNavigate}
      className={({ isActive }) =>
        cx(
          "flex items-center gap-2.5 rounded-md px-2.5 py-1.5 text-[13px] font-medium transition-colors",
          isActive ? "bg-white text-zinc-950 shadow-xs ring-1 ring-zinc-200" : "text-zinc-600 hover:bg-zinc-200/60 hover:text-zinc-950",
        )
      }
    >
      {({ isActive }) => (
        <>
          <Icon className={cx("size-4", isActive ? "text-accent-700" : "text-zinc-400")} />
          {item.label}
        </>
      )}
    </NavLink>
  );
}

type SidebarProps = { open: boolean; onClose: () => void; onOpenPalette: () => void };

export function Sidebar({ open, onClose, onOpenPalette }: SidebarProps) {
  const { user, account } = useSession();
  const logout = useLogout();
  const navigate = useNavigate();
  const isMac = typeof navigator !== "undefined" && /Mac|iPhone|iPad/.test(navigator.platform);

  return (
    <>
      {open ? <div className="fixed inset-0 z-30 bg-zinc-950/30 lg:hidden" onClick={onClose} aria-hidden /> : null}
      <aside
        className={cx(
          "fixed inset-y-0 left-0 z-40 flex w-60 flex-col border-r border-zinc-200 bg-zinc-100 transition-transform lg:translate-x-0",
          open ? "translate-x-0" : "-translate-x-full",
        )}
      >
        <div className="flex h-14 items-center justify-between px-4">
          <Logo />
          <button type="button" className="rounded p-1 text-zinc-500 lg:hidden" onClick={onClose} aria-label="Close menu">
            <X className="size-4" />
          </button>
        </div>
        <div className="px-3 pb-2">
          <button
            type="button"
            onClick={() => {
              onClose();
              onOpenPalette();
            }}
            className="flex w-full items-center gap-2 rounded-md border border-zinc-200 bg-white px-2.5 py-1.5 text-[13px] text-zinc-500 shadow-xs hover:border-zinc-300"
          >
            <Search className="size-3.5" />
            <span className="flex-1 text-left">Search or jump to…</span>
            <Kbd>{isMac ? "⌘K" : "Ctrl K"}</Kbd>
          </button>
        </div>
        <nav className="min-h-0 flex-1 space-y-4 overflow-y-auto px-3 py-2">
          {mainNav.map((group, i) => (
            <div key={group.label ?? i} className="space-y-0.5">
              {group.label ? (
                <div className="px-2.5 pb-1 text-[11px] font-semibold uppercase tracking-wider text-zinc-400">{group.label}</div>
              ) : null}
              {group.items.map((item) => (
                <NavEntry key={item.to} item={item} onNavigate={onClose} />
              ))}
            </div>
          ))}
        </nav>
        <div className="space-y-0.5 border-t border-zinc-200 px-3 py-2">
          {bottomNav.map((item) => (
            <NavEntry key={item.to} item={item} onNavigate={onClose} />
          ))}
        </div>
        <div className="border-t border-zinc-200 p-2">
          <Menu
            align="left"
            up
            className="relative block w-full"
            trigger={(props) => (
              <button
                type="button"
                {...props}
                className="flex w-full items-center gap-2.5 rounded-md px-2 py-1.5 text-left hover:bg-zinc-200/60"
              >
                <span
                  className="flex size-7 shrink-0 items-center justify-center rounded-md text-xs font-semibold text-white"
                  style={{ background: account.brand_color || "#115f42" }}
                >
                  {account.name.slice(0, 1).toUpperCase()}
                </span>
                <span className="min-w-0 flex-1">
                  <span className="block truncate text-[13px] font-medium text-zinc-900">{account.name}</span>
                  <span className="block truncate text-[11px] text-zinc-500">{user.email}</span>
                </span>
                <ChevronsUpDown className="size-3.5 text-zinc-400" />
              </button>
            )}
            items={[
              { label: "Account settings", icon: <Settings className="size-3.5" />, onSelect: () => navigate("/settings") },
              {
                label: "Log out",
                icon: <LogOut className="size-3.5" />,
                onSelect: () => logout.mutate(),
              },
            ]}
          />
        </div>
      </aside>
    </>
  );
}
