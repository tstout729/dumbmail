import { Plug } from "lucide-react";
import type { DashboardFeature } from "../types";

export const feature: DashboardFeature = {
  routes: [
    { path: "integrations", lazy: () => import("./IntegrationsPage").then((m) => ({ Component: m.IntegrationsPage })) },
    { path: "integrations/:id", lazy: () => import("./IntegrationPage").then((m) => ({ Component: m.IntegrationPage })) },
  ],
  nav: [{ label: "Integrations", to: "/integrations", icon: Plug }],
};
