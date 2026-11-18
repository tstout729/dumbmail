import {
  BarChart3,
  Code2,
  FileText,
  Gauge,
  Home,
  LayoutTemplate,
  ListChecks,
  Mail,
  Settings,
  Users,
  Workflow,
  type LucideIcon,
} from "lucide-react";

import type { ComponentType } from "react";
import { featureNav, featureSettingsTabs } from "../../features/registry";

export type NavItem = {
  label: string;
  to: string;
  icon: LucideIcon;
  end?: boolean;
  /** Custom active check, for items whose section contains another item's route. */
  isActive?: (pathname: string) => boolean;
  /** Shown after the label, e.g. an unread count. */
  trailing?: ComponentType;
};
export type NavGroup = { label?: string; items: NavItem[] };

const coreNav: NavGroup[] = [
  {
    items: [
      { label: "Home", to: "/", icon: Home, end: true },
      { label: "Campaigns", to: "/campaigns", icon: Mail },
      { label: "Flows", to: "/flows", icon: Workflow },
      { label: "Sign-up forms", to: "/forms", icon: FileText },
    ],
  },
  {
    label: "Audience",
    items: [
      { label: "Lists & segments", to: "/audience/lists", icon: ListChecks },
      { label: "Profiles", to: "/audience/profiles", icon: Users },
    ],
  },
  {
    label: "Content",
    items: [{ label: "Templates", to: "/content/templates", icon: LayoutTemplate }],
  },
  {
    label: "Analytics",
    items: [
      { label: "Overview", to: "/analytics", icon: BarChart3, end: true },
      { label: "Metrics", to: "/analytics/metrics", icon: Gauge },
    ],
  },
];

/** The core menu plus each feature's entries, added to the group they name (or a new group). */
export const mainNav: NavGroup[] = featureNav.reduce<NavGroup[]>(
  (groups, { group, ...item }) => {
    const at = groups.findIndex((g) => g.label === group);
    if (at === -1) return [...groups, { label: group, items: [item] }];
    return groups.map((g, i) => (i === at ? { ...g, items: [...g.items, item] } : g));
  },
  coreNav,
);

export const bottomNav: NavItem[] = [
  { label: "Developers", to: "/settings/developers", icon: Code2 },
  {
    label: "Settings",
    to: "/settings",
    icon: Settings,
    isActive: (pathname) => pathname.startsWith("/settings") && !pathname.startsWith("/settings/developers"),
  },
];

/** Settings tabs, also listed in the command palette. `title` is the palette label. */
export const settingsNav: { label: string; title: string; to: string; end?: boolean }[] = [
  { label: "Account", title: "Account settings", to: "/settings", end: true },
  { label: "Sending", title: "Sender name and email", to: "/settings/sending" },
  { label: "Domains", title: "Sending and link domains", to: "/settings/domains" },
  { label: "SMS", title: "SMS and phone numbers", to: "/settings/sms" },
  { label: "Website", title: "Website snippet", to: "/settings/website" },
  { label: "Billing", title: "Billing and credit", to: "/settings/billing" },
  { label: "Developers", title: "Developers and API keys", to: "/settings/developers" },
  ...featureSettingsTabs,
];
