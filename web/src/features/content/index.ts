import { Bookmark } from "lucide-react";
import type { DashboardFeature } from "../types";

export const feature: DashboardFeature = {
  routes: [
    { path: "content/saved-blocks", lazy: () => import("./SavedBlocksPage").then((m) => ({ Component: m.SavedBlocksPage })) },
    { path: "content/saved-blocks/:id", lazy: () => import("./SavedBlocksPage").then((m) => ({ Component: m.SavedBlockEditorPage })) },
  ],
  nav: [{ label: "Saved blocks", to: "/content/saved-blocks", icon: Bookmark, group: "Content" }],
};
