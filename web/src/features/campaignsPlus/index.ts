import type { DashboardFeature } from "../types";

// The campaign list, campaign page, report and flow settings import their panels from this folder.
export const feature: DashboardFeature = {
  routes: [{ path: "campaigns/calendar", lazy: () => import("./CalendarPage").then((m) => ({ Component: m.CalendarPage })) }],
  settingsRoutes: [{ path: "tracking", lazy: () => import("./TrackingSettingsPage").then((m) => ({ Component: m.TrackingSettingsPage })) }],
  settingsTabs: [{ label: "Tracking", title: "Link tracking (UTM parameters)", to: "/settings/tracking" }],
};
