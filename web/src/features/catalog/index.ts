import { Package } from "lucide-react";
import type { DashboardFeature } from "../types";

export const feature: DashboardFeature = {
  routes: [
    { path: "products", lazy: () => import("./ProductsPage").then((m) => ({ Component: m.ProductsPage })) },
    { path: "products/feeds", lazy: () => import("./FeedsPage").then((m) => ({ Component: m.FeedsPage })) },
    { path: "products/alerts", lazy: () => import("./AlertsPage").then((m) => ({ Component: m.AlertsPage })) },
    { path: "products/:id", lazy: () => import("./ProductDetailPage").then((m) => ({ Component: m.ProductDetailPage })) },
  ],
  nav: [{ label: "Products", to: "/products", icon: Package, group: "Content" }],
};
