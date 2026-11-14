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

export type NavItem = { label: string; to: string; icon: LucideIcon; end?: boolean };
export type NavGroup = { label?: string; items: NavItem[] };

export const mainNav: NavGroup[] = [
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

export const bottomNav: NavItem[] = [
  { label: "Developers", to: "/settings/developers", icon: Code2 },
  { label: "Settings", to: "/settings", icon: Settings, end: true },
];
