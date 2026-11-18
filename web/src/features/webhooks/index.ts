import type { DashboardFeature } from "../types";

export const feature: DashboardFeature = {
  settingsRoutes: [{ path: "webhooks", lazy: () => import("./WebhooksPage").then((m) => ({ Component: m.WebhooksPage })) }],
  settingsTabs: [{ label: "Webhooks", title: "Webhooks", to: "/settings/webhooks" }],
};
