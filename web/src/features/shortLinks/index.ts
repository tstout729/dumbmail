import type { DashboardFeature } from "../types";

// The text editor imports ShortLinkNote from its own file.
export const feature: DashboardFeature = {
  settingsRoutes: [{ path: "texting", lazy: () => import("./TextingSettingsPage").then((m) => ({ Component: m.TextingSettingsPage })) }],
  settingsTabs: [{ label: "Texting", title: "Short links in text messages", to: "/settings/texting" }],
};
