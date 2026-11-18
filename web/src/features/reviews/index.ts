import { Star } from "lucide-react";
import type { DashboardFeature } from "../types";

export const feature: DashboardFeature = {
  routes: [{ path: "content/reviews", lazy: () => import("./ReviewsPage").then((m) => ({ Component: m.ReviewsPage })) }],
  nav: [{ label: "Reviews", to: "/content/reviews", icon: Star, group: "Content" }],
};
