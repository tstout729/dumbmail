import type { DashboardFeature } from "../types";

export const feature: DashboardFeature = {
  settingsRoutes: [{ path: "shopify", lazy: () => import("./ShopifyPage").then((m) => ({ Component: m.ShopifyPage })) }],
  settingsTabs: [{ label: "Shopify", title: "Shopify store", to: "/settings/shopify" }],
};
