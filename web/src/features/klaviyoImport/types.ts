export type KlaviyoStatus = "surveying" | "ready" | "importing" | "paused" | "done" | "cancelled" | "failed";

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
  /** True when some counts were capped, so the time is "at least". */
  estimate_is_lower_bound?: boolean;
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
  options: { what?: string[]; events_from?: string; metrics?: string[] | null; never_subscribed?: NeverSubscribed };
  phase: string | null;
  progress: Record<string, StepProgress>;
  checklist: Record<string, { done?: boolean }>;
  error: string | null;
  started_at: string | null;
  finished_at: string | null;
  compared_at: string | null;
  created_at: string;
};

export type ChecklistItem = { key: string; label: string; done: boolean; auto: boolean; detail: string; link?: string; checked_at?: string | null };

export type ReviewKind = "campaign" | "flow" | "template" | "form";

export type KlaviyoReview = { kind: ReviewKind; id: string; name: string | null; notes: string[] };

export type KlaviyoState = {
  import: KlaviyoImport | null;
  reviews: KlaviyoReview[];
  issues: Record<string, number>;
  checklist: { items: ChecklistItem[]; done: number; total: number };
  steps: string[];
  choosable: string[];
  scopes: { label: string; scope: string }[];
};

export type LineState = "match" | "mismatch" | "not_verified" | "info";

export type ReportLine = { section: string; label: string; klaviyo: number | null; dumbmail: number | null; state: LineState; ok: boolean; diagnostic: boolean; note: string };

export type KlaviyoReport = {
  compared_at: string | null;
  all_ok: boolean;
  mismatches: number;
  not_verified: number;
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

export type NeverSubscribed = "send" | "hold";

export type StartOptions = { confirm: boolean; what: string[]; events_from: string; never_subscribed: NeverSubscribed };
