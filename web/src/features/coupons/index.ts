import { TicketPercent } from "lucide-react";
import type { DashboardFeature } from "../types";

export const feature: DashboardFeature = {
  routes: [
    { path: "content/coupons", lazy: () => import("./CouponsPage").then((m) => ({ Component: m.CouponsPage })) },
    { path: "content/coupons/:id", lazy: () => import("./CouponPage").then((m) => ({ Component: m.CouponPage })) },
  ],
  nav: [{ label: "Coupons", to: "/content/coupons", icon: TicketPercent, group: "Content" }],
};
