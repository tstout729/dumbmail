import type { RouteObject } from "react-router";
import { feature as agent } from "./agent";
import { feature as audiences } from "./audiences";
import { feature as catalog } from "./catalog";
import { feature as content } from "./content";
import { feature as coupons } from "./coupons";
import { feature as hub } from "./hub";
import { feature as inbox } from "./inbox";
import { feature as integrations } from "./integrations";
import { feature as klaviyoImport } from "./klaviyoImport";
import { feature as objects } from "./objects";
import { feature as push } from "./push";
import { feature as reports } from "./reports";
import { feature as reviews } from "./reviews";
import { feature as sendtime } from "./sendtime";
import { feature as shopify } from "./shopify";
import { feature as team } from "./team";
import { feature as transactional } from "./transactional";
import { feature as warehouse } from "./warehouse";
import { feature as webhooks } from "./webhooks";
import { feature as whatsapp } from "./whatsapp";
import type { DashboardFeature, FeatureNavItem, FeatureSettingsTab } from "./types";

// Each feature keeps its dashboard code in its own folder and is listed here once, so adding
// pages or menu items to a feature doesn't touch the shared router or sidebar.
const features: DashboardFeature[] = [agent, audiences, catalog, content, coupons, hub, inbox, integrations, klaviyoImport, objects, push, reports, reviews, sendtime, shopify, team, transactional, warehouse, webhooks, whatsapp];

export const featureRoutes: RouteObject[] = features.flatMap((f) => f.routes ?? []);
export const featurePublicRoutes: RouteObject[] = features.flatMap((f) => f.publicRoutes ?? []);
export const featureSettingsRoutes: RouteObject[] = features.flatMap((f) => f.settingsRoutes ?? []);
export const featureNav: FeatureNavItem[] = features.flatMap((f) => f.nav ?? []);
export const featureSettingsTabs: FeatureSettingsTab[] = features.flatMap((f) => f.settingsTabs ?? []);
