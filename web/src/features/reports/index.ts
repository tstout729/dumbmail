import { FileBarChart, Filter, FolderDown, MailCheck } from "lucide-react";
import type { DashboardFeature } from "../types";

export const feature: DashboardFeature = {
  routes: [
    { path: "analytics/reports", lazy: () => import("./ReportsPage").then((m) => ({ Component: m.ReportsPage })) },
    { path: "analytics/reports/new", lazy: () => import("./ReportPage").then((m) => ({ Component: m.ReportPage })) },
    { path: "analytics/reports/:id", lazy: () => import("./ReportPage").then((m) => ({ Component: m.ReportPage })) },
    { path: "analytics/deliverability", lazy: () => import("./DeliverabilityPage").then((m) => ({ Component: m.DeliverabilityPage })) },
    { path: "analytics/cohorts", lazy: () => import("./CohortsFunnelsPage").then((m) => ({ Component: m.CohortsFunnelsPage })) },
    { path: "analytics/exports", lazy: () => import("./ExportsPage").then((m) => ({ Component: m.ExportsPage })) },
  ],
  settingsRoutes: [{ path: "attribution", lazy: () => import("./AttributionSettingsPage").then((m) => ({ Component: m.AttributionSettingsPage })) }],
  nav: [
    { group: "Analytics", label: "Reports", to: "/analytics/reports", icon: FileBarChart },
    { group: "Analytics", label: "Deliverability", to: "/analytics/deliverability", icon: MailCheck },
    { group: "Analytics", label: "Cohorts & funnels", to: "/analytics/cohorts", icon: Filter },
    { group: "Analytics", label: "Exports", to: "/analytics/exports", icon: FolderDown },
  ],
  settingsTabs: [{ label: "Attribution", title: "Revenue attribution settings", to: "/settings/attribution" }],
};
