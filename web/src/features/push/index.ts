import type { DashboardFeature } from "../types";

// The campaign and flow editors import PushEditor and PushReportCard from their own files.
export const feature: DashboardFeature = {
  settingsRoutes: [{ path: "push", lazy: () => import("./PushSettingsPage").then((m) => ({ Component: m.PushSettingsPage })) }],
  settingsTabs: [{ label: "Push", title: "Web and mobile push notifications", to: "/settings/push" }],
};
