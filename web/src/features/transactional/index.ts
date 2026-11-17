import { Receipt } from "lucide-react";
import type { DashboardFeature } from "../types";

export const feature: DashboardFeature = {
  routes: [{ path: "transactional", lazy: () => import("./TransactionalPage").then((m) => ({ Component: m.TransactionalPage })) }],
  settingsRoutes: [{ path: "transactional", lazy: () => import("./TransactionalSettingsPage").then((m) => ({ Component: m.TransactionalSettingsPage })) }],
  nav: [{ label: "Transactional", to: "/transactional", icon: Receipt }],
  settingsTabs: [{ label: "Transactional", title: "Turn transactional email on or off", to: "/settings/transactional" }],
};
