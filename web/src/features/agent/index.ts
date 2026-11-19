import type { DashboardFeature } from "../types";

export const feature: DashboardFeature = {
  routes: [
    { path: "inbox/agent", lazy: () => import("./AgentConversationsPage").then((m) => ({ Component: m.AgentConversationsPage })) },
    { path: "inbox/agent/:id", lazy: () => import("./AgentConversationsPage").then((m) => ({ Component: m.AgentConversationsPage })) },
  ],
  settingsRoutes: [{ path: "shopping-agent", lazy: () => import("./AgentSettingsPage").then((m) => ({ Component: m.AgentSettingsPage })) }],
  settingsTabs: [{ label: "Shopping agent", title: "Shopping agent (AI answers for shoppers)", to: "/settings/shopping-agent" }],
};
