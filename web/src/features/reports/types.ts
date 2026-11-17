export type MeasureKind = "count" | "rate" | "currency" | "number";

export type ReportFilter = { field: string; values: string[]; exclude?: boolean; key?: string };

export type ReportRange = { days?: number; start?: string; end?: string };

export type ReportSpec = {
  source: string;
  measures?: string[];
  group_by?: string[];
  filters?: ReportFilter[];
  range?: ReportRange;
  compare?: boolean;
  metric?: string;
  property?: string;
  sort?: { by: string; desc?: boolean };
  limit?: number;
};

export type CatalogSource = {
  key: string;
  label: string;
  description: string;
  group_by: { key: string; label: string }[];
  filters: string[];
  measures: { key: string; label: string; kind: MeasureKind }[];
  default_measures: string[];
};

export type Catalog = { sources: CatalogSource[] };

export type ReportColumn = { key: string; label: string; kind: MeasureKind | "group" };

export type ReportRow = { keys: string[]; labels: string[]; values: Record<string, number> };

export type ReportResult = {
  spec: ReportSpec;
  timezone: string;
  range: { start: string; end: string };
  previous_range: { start: string; end: string } | null;
  columns: ReportColumn[];
  rows: ReportRow[];
  totals: Record<string, number>;
  previous: Record<string, number> | null;
  truncated: boolean;
  attribution: string | null;
};

export type Schedule = "weekly" | "monthly";

export type SavedReport = {
  id: string;
  builtin: boolean;
  name: string;
  description: string;
  spec: ReportSpec;
  schedule: Schedule | null;
  recipients: string[];
  /** Each recipient, and whether they've confirmed their address for report emails. */
  recipient_status: RecipientStatus[];
  next_run_at: string | null;
  last_sent_at: string | null;
  last_error: string | null;
  created_at?: string;
  updated_at?: string;
};

export type ReportList = { builtin: SavedReport[]; saved: SavedReport[] };

export type ReportInput = {
  name?: string;
  description?: string;
  spec?: ReportSpec;
  schedule?: Schedule | null;
  recipients?: string[];
};

/** confirmed: gets report emails. waiting: a confirmation link was emailed. not_asked: neither yet. */
export type ReportEmailStatus = "confirmed" | "waiting" | "not_asked";

export type RecipientStatus = { id: string; email: string; name: string | null; status: ReportEmailStatus };

export type Member = { id: string; email: string; name: string | null; role: string; report_emails: ReportEmailStatus };

export type AttributionSettings = {
  email_open_window_hours: number;
  email_click_window_hours: number;
  sms_click_window_hours: number;
  count_opens: boolean;
  model: "last_touch" | "first_touch";
  conversion_metrics: string[];
  description: string;
  is_default: boolean;
};

export type AttributionPatch = Partial<Omit<AttributionSettings, "description" | "is_default">>;

export type AttributionTotals = {
  conversions: number;
  revenue: number;
  attributed_conversions: number;
  attributed_revenue: number;
  campaign_revenue: number;
  flow_revenue: number;
  email_revenue: number;
  sms_revenue: number;
};

export type AttributionPreview = {
  days: number;
  current: { settings: AttributionSettings; totals: AttributionTotals };
  proposed: { settings: AttributionSettings; totals: AttributionTotals };
};

export type CohortCell = { month: number; customers: number; rate: number; revenue: number; cumulative_revenue_per_person: number };
export type Cohorts = {
  by: "first_purchase" | "signup";
  months: number;
  timezone: string;
  metrics: string[];
  rate_label: string;
  cohorts: { cohort: string; size: number; cells: CohortCell[] }[];
};

export type FunnelSplit = { type: "event_property" | "profile_property" | "source"; key?: string };
export type FunnelQuery = { steps: string[]; window_days?: number; range?: ReportRange; split_by?: FunnelSplit };
export type FunnelStep = { step: number; metric: string; people: number; conversion_from_previous: number; conversion_from_start: number; dropped: number };
export type Funnel = {
  steps: string[];
  window_days: number;
  total: FunnelStep[];
  splits: { key: string; label: string; steps: FunnelStep[] }[];
};

export type Rates = {
  sent: number;
  delivered: number;
  opens: number;
  clicks: number;
  unsubscribes: number;
  bounces: number;
  hard_bounces: number;
  soft_bounces: number;
  complaints: number;
  delivery_rate: number;
  open_rate: number;
  click_rate: number;
  bounce_rate: number;
  hard_bounce_rate: number;
  soft_bounce_rate: number;
  complaint_rate: number;
  unsubscribe_rate: number;
};

export type Health = {
  score: number | null;
  grade: "good" | "fair" | "poor" | "not_enough_data";
  flags: { level: "info" | "warning" | "critical"; text: string }[];
  note?: string;
};

export type AuthStatus = { dkim: "pass" | "pending" | "missing"; spf: "pass" | "pending" | "missing"; dmarc: "pass" | "pending" | "missing" };

export type Deliverability = {
  days: number;
  interval: "day" | "week";
  overall: Rates & { health: Health };
  providers: (Rates & { provider: string; label: string; health: Health })[];
  series: (Rates & { date: string; provider: string })[];
  sending_domains: (Rates & { domain: string | null; label: string; health: Health; authentication: AuthStatus | null })[];
  authentication: (AuthStatus & { domain: string; status: string; verified_at: string | null })[];
  provider_note: string;
};

export type BenchmarkMetric = {
  key: string;
  label: string;
  you: number;
  trailing_90_days: number;
  up_is_good: boolean;
  industry?: number;
  industry_range?: [number, number];
  limit?: { good_below: number; review_at: number; source: string; url: string; checked: string; text: string };
  verdict?: "better" | "worse" | "not_enough_data";
};

export type Benchmarks = {
  industry: { key: string; label: string };
  industries: { key: string; label: string }[];
  sent_last_30_days: number;
  sent_last_90_days: number;
  metrics: BenchmarkMetric[];
  sources: { engagement: { name: string; url: string; published: string; period: string; note: string } };
  note: string;
};

export type ExportKind = "profiles" | "events" | "campaign_recipients" | "suppressions";

export type ExportRequest = {
  kind: ExportKind;
  scope?: "all" | "list" | "segment";
  list_id?: string;
  segment_id?: string;
  columns?: string[];
  properties?: string[];
  metric?: string;
  start?: string;
  end?: string;
  campaign_id?: string;
};

export type ExportFile = {
  id: string;
  kind: ExportKind | "report";
  status: "queued" | "running" | "done" | "failed" | "expired";
  filename: string;
  row_count: number;
  size_bytes: number;
  error: string | null;
  created_at: string;
  finished_at: string | null;
  expires_at: string;
  download_url: string | null;
};
