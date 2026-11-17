import type { DashboardFeature } from "../types";

// The campaign page imports SendOptionsPanel and SendProgressCard from their own files.
export const feature: DashboardFeature = {
  settingsRoutes: [{ path: "send-times", lazy: () => import("./SendSettingsPage").then((m) => ({ Component: m.SendSettingsPage })) }],
  settingsTabs: [{ label: "Send times", title: "Text message hours and time zones", to: "/settings/send-times" }],
};
