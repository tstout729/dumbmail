export type KlaviyoStatus = "surveying" | "ready" | "importing" | "paused" | "syncing" | "done" | "cancelled" | "failed";

export type SurveyCount = { count: number | null; exact: boolean; error?: string };

export type KlaviyoSurvey = {
  profiles: SurveyCount;
  lists: SurveyCount;
  segments: SurveyCount;
  metrics: SurveyCount;
  templates: SurveyCount;
  campaigns: SurveyCount;
  flows: SurveyCount;
  forms: SurveyCount;
  catalog_items: SurveyCount;
  coupons: SurveyCount;
  suppressions: SurveyCount & { by_reason: Record<string, SurveyCount> };
  events: { estimate: number; since: string | null; by_metric: { id: string; name: string; estimate: number }[] };
  all_metrics: { id: string; name: string; integration: string }[];
  estimate_minutes: number;
  missing_scopes: string[];
};

export type StepProgress = { done: number; skipped: number; finished: boolean };

export type KlaviyoImport = {
  id: string;
  status: KlaviyoStatus;
  key_hint: string;
  key_stored: boolean;
  klaviyo_account: { name?: string | null; id?: string; timezone?: string | null };
  survey: KlaviyoSurvey | null;
  options: { what?: string[]; events_from?: string; metrics?: string[] | null };
  phase: string | null;
  progress: Record<string, StepProgress>;
  checklist: Record<string, { done?: boolean }>;
  sync_enabled: boolean;
  last_sync_at: string | null;
  error: string | null;
  started_at: string | null;
  finished_at: string | null;
  compared_at: string | null;
  created_at: string;
};

export type ChecklistItem = { key: string; label: string; done: boolean; auto: boolean; detail: string; link?: string; checked_at?: string | null };

export type KlaviyoState = {
  import: KlaviyoImport | null;
  issues: Record<string, number>;
  checklist: { items: ChecklistItem[]; done: number; total: number };
  steps: string[];
  choosable: string[];
  scopes: { label: string; scope: string }[];
  sync_worst_case_delay: string;
};

export type ReportLine = { section: string; label: string; klaviyo: number | null; dumbmail: number | null; ok: boolean; note: string };

export type KlaviyoReport = {
  compared_at: string | null;
  all_ok: boolean;
  lines_not_ok: number;
  lines: ReportLine[];
  issues_by_step: { step: string; kind: string; count: number }[];
};

export type KlaviyoIssue = { id: number; step: string; kind: string; item: string; message: string; local_id: string | null };

export type KlaviyoCampaign = {
  id: string;
  name: string;
  channel: string;
  status: string;
  sent_at: string | null;
  subject: string | null;
  audience: { included: string[]; excluded: string[] } | null;
  stats: Partial<Record<"recipients" | "delivered" | "opens_unique" | "clicks_unique" | "unsubscribes" | "conversions" | "conversion_value", number>> | null;
};

export type StartOptions = { confirm: boolean; what: string[]; events_from: string; sync: boolean };
