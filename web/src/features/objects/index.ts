import { Boxes } from "lucide-react";
import type { DashboardFeature } from "../types";

export const feature: DashboardFeature = {
  routes: [
    { path: "audience/objects", lazy: () => import("./ObjectsPage").then((m) => ({ Component: m.ObjectsPage })) },
    { path: "audience/objects/:type", lazy: () => import("./ObjectTypePage").then((m) => ({ Component: m.ObjectTypePage })) },
    { path: "audience/objects/:type/:id", lazy: () => import("./ObjectPage").then((m) => ({ Component: m.ObjectPage })) },
  ],
  nav: [{ label: "Objects", to: "/audience/objects", icon: Boxes, group: "Audience" }],
};
