import type { DashboardFeature } from "../types";

export const feature: DashboardFeature = {
  settingsRoutes: [
    {
      path: "ad-audiences",
      lazy: () => import("./AdAudiencesPage").then((m) => ({ Component: m.AdAudiencesPage })),
    },
  ],
  settingsTabs: [{ label: "Ad audiences", title: "Ad audiences (Meta, Google, TikTok)", to: "/settings/ad-audiences" }],
};
