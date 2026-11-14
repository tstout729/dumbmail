import { createBrowserRouter } from "react-router";
import { AppShell, RedirectIfAuthed, RequireAuth } from "./components/shell/AppShell";
import { RootLayout } from "./RootLayout";

// Each page is its own chunk; navigation keeps the current page visible until the next one loads.
export const router = createBrowserRouter([
  {
    Component: RootLayout,
    HydrateFallback: () => null,
    children: [
      {
        Component: RedirectIfAuthed,
        children: [
          { path: "/login", lazy: () => import("./pages/auth/LoginPage").then((m) => ({ Component: m.LoginPage })) },
          { path: "/signup", lazy: () => import("./pages/auth/SignupPage").then((m) => ({ Component: m.SignupPage })) },
        ],
      },
      {
        Component: RequireAuth,
        children: [
          {
            path: "/onboarding",
            lazy: () => import("./pages/onboarding/OnboardingPage").then((m) => ({ Component: m.OnboardingPage })),
          },
          {
            Component: AppShell,
            children: [
              { index: true, lazy: () => import("./pages/home/HomePage").then((m) => ({ Component: m.HomePage })) },
              { path: "campaigns", lazy: () => import("./pages/campaigns/CampaignsPage").then((m) => ({ Component: m.CampaignsPage })) },
              { path: "campaigns/:id", lazy: () => import("./pages/campaigns/CampaignPage").then((m) => ({ Component: m.CampaignPage })) },
              { path: "flows", lazy: () => import("./pages/flows/FlowsPage").then((m) => ({ Component: m.FlowsPage })) },
              { path: "flows/:id", lazy: () => import("./pages/flows/FlowBuilderPage").then((m) => ({ Component: m.FlowBuilderPage })) },
              { path: "forms", lazy: () => import("./pages/forms/FormsPage").then((m) => ({ Component: m.FormsPage })) },
              { path: "forms/:id", lazy: () => import("./pages/forms/FormEditorPage").then((m) => ({ Component: m.FormEditorPage })) },
              { path: "audience/lists", lazy: () => import("./pages/audience/ListsPage").then((m) => ({ Component: m.ListsPage })) },
              { path: "audience/lists/:id", lazy: () => import("./pages/audience/ListDetailPage").then((m) => ({ Component: m.ListDetailPage })) },
              { path: "audience/segments/:id", lazy: () => import("./pages/audience/SegmentPage").then((m) => ({ Component: m.SegmentPage })) },
              { path: "audience/profiles", lazy: () => import("./pages/audience/ProfilesPage").then((m) => ({ Component: m.ProfilesPage })) },
              { path: "audience/profiles/:id", lazy: () => import("./pages/audience/ProfileDetailPage").then((m) => ({ Component: m.ProfileDetailPage })) },
              { path: "content/templates", lazy: () => import("./pages/content/TemplatesPage").then((m) => ({ Component: m.TemplatesPage })) },
              { path: "content/templates/:id", lazy: () => import("./pages/content/TemplateEditorPage").then((m) => ({ Component: m.TemplateEditorPage })) },
              { path: "analytics", lazy: () => import("./pages/analytics/AnalyticsPage").then((m) => ({ Component: m.AnalyticsPage })) },
              { path: "analytics/metrics", lazy: () => import("./pages/analytics/MetricsPage").then((m) => ({ Component: m.MetricsPage })) },
              {
                path: "settings",
                lazy: () => import("./pages/settings/SettingsLayout").then((m) => ({ Component: m.SettingsLayout })),
                children: [
                  { index: true, lazy: () => import("./pages/settings/AccountSettingsPage").then((m) => ({ Component: m.AccountSettingsPage })) },
                  { path: "sending", lazy: () => import("./pages/settings/SendingSettingsPage").then((m) => ({ Component: m.SendingSettingsPage })) },
                  { path: "domains", lazy: () => import("./pages/settings/DomainsPage").then((m) => ({ Component: m.DomainsPage })) },
                  { path: "sms", lazy: () => import("./pages/settings/SmsPage").then((m) => ({ Component: m.SmsPage })) },
                  { path: "website", lazy: () => import("./pages/settings/WebsitePage").then((m) => ({ Component: m.WebsitePage })) },
                  {
                    path: "billing",
                    lazy: () => import("./pages/settings/BillingPage").then((m) => ({ Component: m.BillingPage, loader: m.billingLoader })),
                  },
                  { path: "developers", lazy: () => import("./pages/settings/DevelopersPage").then((m) => ({ Component: m.DevelopersPage })) },
                ],
              },
              { path: "*", lazy: () => import("./pages/NotFoundPage").then((m) => ({ Component: m.NotFoundPage })) },
            ],
          },
        ],
      },
    ],
  },
]);
