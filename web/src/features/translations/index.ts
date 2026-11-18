import type { DashboardFeature } from "../types";

// The email and text editors open TranslationsDrawer; segments use LanguageConditionFields; the
// campaign report shows LanguageResults.
export const feature: DashboardFeature = {
  settingsRoutes: [{ path: "languages", lazy: () => import("./LanguagesPage").then((m) => ({ Component: m.LanguagesPage })) }],
  settingsTabs: [{ label: "Languages", title: "Languages, glossary and page wording", to: "/settings/languages" }],
};
