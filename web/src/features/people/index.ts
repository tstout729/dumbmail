import { Copy, Lock, ShieldOff } from "lucide-react";
import type { DashboardFeature } from "../types";

// Profile tools: suppression list, possible duplicates and privacy requests. The profile page,
// the Profiles page and list/segment pages pull in ProfilePanels and BulkActions from here.
export const feature: DashboardFeature = {
  routes: [
    { path: "audience/suppressions", lazy: () => import("./SuppressionsPage").then((m) => ({ Component: m.SuppressionsPage })) },
    { path: "audience/duplicates", lazy: () => import("./DuplicatesPage").then((m) => ({ Component: m.DuplicatesPage })) },
    { path: "audience/privacy", lazy: () => import("./PrivacyPage").then((m) => ({ Component: m.PrivacyPage })) },
  ],
  nav: [
    { group: "Audience", label: "Suppression list", to: "/audience/suppressions", icon: ShieldOff },
    { group: "Audience", label: "Duplicates", to: "/audience/duplicates", icon: Copy },
    { group: "Audience", label: "Privacy requests", to: "/audience/privacy", icon: Lock },
  ],
};
