import { Navigate, NavLink, Outlet, useSearchParams } from "react-router";
import { settingsNav } from "../../components/shell/nav";
import { Page, PageHeader, cx } from "../../components/ui";

// v0.1 linked to /settings?tab=…; those links still work.
const legacyTabs = new Set(["sending", "sms", "billing", "domains", "website", "developers"]);

export function SettingsLayout() {
  const [params] = useSearchParams();
  const legacy = params.get("tab");
  if (legacy && legacyTabs.has(legacy)) return <Navigate to={`/settings/${legacy}`} replace />;

  return (
    <Page>
      <PageHeader title="Settings" />
      <nav className="mb-5 flex gap-1 overflow-x-auto border-b border-zinc-200" aria-label="Settings sections">
        {settingsNav.map((t) => (
          <NavLink
            key={t.to}
            to={t.to}
            end={t.end}
            className={({ isActive }) =>
              cx(
                "-mb-px whitespace-nowrap border-b-2 px-3 py-2 text-sm font-medium transition-colors",
                isActive ? "border-accent-700 text-zinc-950" : "border-transparent text-zinc-500 hover:text-zinc-900",
              )
            }
          >
            {t.label}
          </NavLink>
        ))}
      </nav>
      <div className="max-w-4xl">
        <Outlet />
      </div>
    </Page>
  );
}
