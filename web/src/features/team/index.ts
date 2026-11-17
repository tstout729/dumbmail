import type { DashboardFeature } from "../types";

// Team members, roles, several accounts per login, two-factor login, sessions, single sign-on and
// the activity log. The sidebar's account menu (AccountMenu), the sign-in gate (SignInGate) and the
// Developers page's API keys (ApiKeysCard) are imported where they're shown.
export const feature: DashboardFeature = {
  settingsRoutes: [
    { path: "team", lazy: () => import("./TeamPage").then((m) => ({ Component: m.TeamPage })) },
    { path: "security", lazy: () => import("./SecurityPage").then((m) => ({ Component: m.SecurityPage })) },
  ],
  settingsTabs: [
    { label: "Team", title: "Team members and roles", to: "/settings/team" },
    { label: "Security", title: "Two-factor login, sessions, single sign-on and activity log", to: "/settings/security" },
  ],
  publicRoutes: [
    { path: "invite/:token", lazy: () => import("./PublicPages").then((m) => ({ Component: m.InvitePage })) },
    { path: "forgot-password", lazy: () => import("./PublicPages").then((m) => ({ Component: m.ForgotPasswordPage })) },
    { path: "reset-password", lazy: () => import("./PublicPages").then((m) => ({ Component: m.ResetPasswordPage })) },
  ],
};
