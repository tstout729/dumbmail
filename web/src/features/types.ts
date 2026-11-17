import type { RouteObject } from "react-router";
import type { NavItem } from "../components/shell/nav";

/** A sidebar entry. `group` is the sidebar group's label ("Audience", "Content", "Analytics", or a new one); leave it out for the top group. */
export type FeatureNavItem = NavItem & { group?: string };
/** A settings tab, also listed in the command palette under `title`. */
export type FeatureSettingsTab = { label: string; title: string; to: string; end?: boolean };

/**
 * What a feature adds to the dashboard. Pages load lazily:
 * `{ path: "integrations", lazy: () => import("./IntegrationsPage").then((m) => ({ Component: m.IntegrationsPage })) }`.
 */
export type DashboardFeature = {
  /** Pages inside the app shell (paths relative to "/"). */
  routes?: RouteObject[];
  /** Pages under /settings (paths relative to "/settings"). */
  settingsRoutes?: RouteObject[];
  nav?: FeatureNavItem[];
  settingsTabs?: FeatureSettingsTab[];
};
