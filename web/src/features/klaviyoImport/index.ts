import type { DashboardFeature } from "../types";

export const feature: DashboardFeature = {
  settingsRoutes: [{ path: "klaviyo", lazy: () => import("./KlaviyoImportPage").then((m) => ({ Component: m.KlaviyoImportPage })) }],
  settingsTabs: [{ label: "Move from Klaviyo", title: "Move from Klaviyo", to: "/settings/klaviyo" }],
};
