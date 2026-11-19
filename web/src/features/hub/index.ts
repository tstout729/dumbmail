import type { DashboardFeature } from "../types";

export const feature: DashboardFeature = {
  settingsRoutes: [{ path: "customer-hub", lazy: () => import("./HubSettingsPage").then((m) => ({ Component: m.HubSettingsPage })) }],
  settingsTabs: [{ label: "Customer hub", title: "Customer hub (shopper account page)", to: "/settings/customer-hub" }],
};
