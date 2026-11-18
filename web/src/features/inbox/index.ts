import { MessagesSquare } from "lucide-react";
import type { DashboardFeature } from "../types";
import { UnreadBadge } from "./UnreadBadge";

export const feature: DashboardFeature = {
  routes: [
    { path: "inbox", lazy: () => import("./InboxPage").then((m) => ({ Component: m.InboxPage })) },
    { path: "inbox/keywords", lazy: () => import("./KeywordsPage").then((m) => ({ Component: m.KeywordsPage })) },
    { path: "inbox/:id", lazy: () => import("./InboxPage").then((m) => ({ Component: m.InboxPage })) },
  ],
  settingsRoutes: [{ path: "inbox", lazy: () => import("./InboxSettingsPage").then((m) => ({ Component: m.InboxSettingsPage })) }],
  nav: [{ label: "Inbox", to: "/inbox", icon: MessagesSquare, trailing: UnreadBadge }],
  settingsTabs: [{ label: "Inbox", title: "SMS inbox, keywords and automatic replies", to: "/settings/inbox" }],
};
