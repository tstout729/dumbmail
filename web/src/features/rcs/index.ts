import type { DashboardFeature } from "../types";

// RCS adds no pages of its own: Settings → SMS shows RcsSettingsCard, the SMS editor shows
// RichVersionEditor and RcsPreviewSwitch, and campaign and flow reports show RcsReportCard.
export const feature: DashboardFeature = {};
