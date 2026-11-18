import type { DashboardFeature } from "../types";

export const feature: DashboardFeature = {
  settingsRoutes: [{ path: "data-export", lazy: () => import("./DataExportPage").then((m) => ({ Component: m.DataExportPage })) }],
  settingsTabs: [{ label: "Data export", title: "Data export", to: "/settings/data-export" }],
};
