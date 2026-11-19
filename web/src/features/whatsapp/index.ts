import type { DashboardFeature } from "../types";

// The campaign and flow editors import WhatsappEditor and WhatsappReportCard from their own files;
// the texting inbox links to the WhatsApp one.
export const feature: DashboardFeature = {
  routes: [
    { path: "inbox/whatsapp", lazy: () => import("./WhatsappInboxPage").then((m) => ({ Component: m.WhatsappInboxPage })) },
    { path: "inbox/whatsapp/:id", lazy: () => import("./WhatsappInboxPage").then((m) => ({ Component: m.WhatsappInboxPage })) },
  ],
  settingsRoutes: [{ path: "whatsapp", lazy: () => import("./WhatsappSettingsPage").then((m) => ({ Component: m.WhatsappSettingsPage })) }],
  settingsTabs: [{ label: "WhatsApp", title: "WhatsApp Business connection and templates", to: "/settings/whatsapp" }],
};
