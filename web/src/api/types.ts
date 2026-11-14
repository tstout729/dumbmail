// Types mirror docs/API.md (v0.1 contract). Keep them in sync with that file.

export type Consent = "subscribed" | "unsubscribed" | "never_subscribed";

export type ListResponse<T> = { data: T[]; total: number };

export type ApiErrorCode =
  | "not_found"
  | "bad_request"
  | "unauthorized"
  | "forbidden"
  | "conflict"
  | "limit_reached"
  | "internal";

export type Profile = {
  id: string;
  email: string | null;
  phone_number: string | null;
  external_id: string | null;
  first_name: string | null;
  last_name: string | null;
  properties: Record<string, unknown>;
  email_consent: Consent;
  sms_consent: Consent;
  created_at: string;
  updated_at: string;
  last_event_at: string | null;
};

export type Plan = "free" | "pro" | "scale" | "self_hosted";

export type Onboarding = {
  brand: boolean;
  sender: boolean;
  contacts: boolean;
  first_flow: boolean;
  dismissed: boolean;
};

export type Account = {
  id: string;
  public_key: string;
  name: string;
  website: string | null;
  from_name: string | null;
  from_email: string | null;
  reply_to: string | null;
  sms_from: string | null;
  timezone: string;
  address: string | null;
  brand_color: string;
  logo_url: string | null;
  plan: Plan;
  onboarding: Onboarding;
  created_at: string;
};

export type AccountPatch = Partial<
  Pick<
    Account,
    | "name"
    | "website"
    | "from_name"
    | "from_email"
    | "reply_to"
    | "sms_from"
    | "timezone"
    | "address"
    | "brand_color"
    | "logo_url"
  >
> & { onboarding?: Partial<Onboarding> };

export type User = { id: string; email: string; name: string | null };

export type Session = { user: User; account: Account };

// ---- Segments and conditions ----

export type PropOp =
  | "eq"
  | "neq"
  | "contains"
  | "not_contains"
  | "starts_with"
  | "exists"
  | "not_exists"
  | "gt"
  | "gte"
  | "lt"
  | "lte"
  | "in"
  | "within_last_days"
  | "more_than_days_ago";

export type FieldFilter = { field: string; op: PropOp; value?: unknown };

export type EventCountOp = "at_least" | "at_most" | "exactly" | "zero";

export type PropertyCondition = { type: "property"; field: string; op: PropOp; value?: unknown };
export type EventCondition = {
  type: "event";
  metric: string;
  op: EventCountOp;
  count?: number;
  within_days?: number;
  where?: FieldFilter[];
};
export type ListCondition = { type: "list"; list_id: string; op: "in" | "not_in" };
export type ConsentCondition = { type: "consent"; channel: "email" | "sms"; status: Consent };
export type GroupCondition = { type: "group"; match: "all" | "any"; conditions: Condition[] };

export type Condition =
  | PropertyCondition
  | EventCondition
  | ListCondition
  | ConsentCondition
  | GroupCondition;

export type SegmentDefinition = { match: "all" | "any"; conditions: Condition[] };

export type AudienceRef = { type: "list" | "segment"; id: string };
export type Audience = { include: AudienceRef[]; exclude: AudienceRef[] };

// ---- Content ----

export type Align = "left" | "center" | "right";

export type EmailBlock =
  | { type: "heading"; text: string; level?: 1 | 2 | 3; align?: Align }
  | { type: "text"; html: string; align?: Align }
  | { type: "image"; src: string; alt?: string; href?: string; width?: number }
  | { type: "button"; text: string; href: string; color?: string; align?: Align }
  | { type: "divider" }
  | { type: "spacer"; height?: number }
  | { type: "columns"; columns: EmailBlock[][] }
  | { type: "product"; title: string; image?: string; price?: string; href: string; button_text?: string }
  | { type: "html"; html: string };

export type EmailBlockType = EmailBlock["type"];

export type EmailTheme = { brand_color?: string; background?: string; font?: string };

export type EmailContent = {
  subject: string;
  preheader?: string;
  from_name?: string;
  from_email?: string;
  reply_to?: string;
  blocks?: EmailBlock[];
  html?: string;
  theme?: EmailTheme;
};

export type SmsContent = { body: string; media_url?: string };

export type Channel = "email" | "sms";

export type MessageStats = {
  recipients: number;
  delivered: number;
  bounced: number;
  failed: number;
  unique_opens: number;
  unique_clicks: number;
  unsubscribes: number;
  complaints: number;
  open_rate: number;
  click_rate: number;
  orders: number;
  revenue: number;
};

// ---- Campaigns ----

export type CampaignStatus = "draft" | "scheduled" | "sending" | "sent" | "cancelled" | "failed";

export type Campaign = {
  id: string;
  name: string;
  channel: Channel;
  status: CampaignStatus;
  audience: Audience;
  content: EmailContent | SmsContent;
  send_at: string | null;
  sent_at: string | null;
  created_at: string;
  updated_at: string;
  stats: MessageStats;
};

export type CampaignReport = {
  stats: MessageStats;
  links: { url: string; clicks: number }[];
  series: { hour: string; opens: number; clicks: number }[];
};

// ---- Flows ----

export type FlowTrigger =
  | { type: "metric"; metric: string; filters?: FieldFilter[] }
  | { type: "list"; list_id: string }
  | { type: "segment"; segment_id: string }
  | { type: "date_property"; field: string; offset_days?: number };

export type DelayUnit = "minutes" | "hours" | "days";

export type DelayNode = { id: string; type: "delay"; amount: number; unit: DelayUnit; next: string | null };
export type EmailNode = { id: string; type: "email"; name?: string; content: EmailContent; next: string | null };
export type SmsNode = { id: string; type: "sms"; name?: string; content: SmsContent; next: string | null };
export type ConditionNode = {
  id: string;
  type: "condition";
  condition: SegmentDefinition;
  yes: string | null;
  no: string | null;
};
export type UpdateProfileNode = {
  id: string;
  type: "update_profile";
  properties: Record<string, unknown>;
  next: string | null;
};
export type ListActionNode = {
  id: string;
  type: "add_to_list" | "remove_from_list";
  list_id: string;
  next: string | null;
};
export type WebhookNode = {
  id: string;
  type: "webhook";
  url: string;
  method?: "POST" | "GET";
  body?: unknown;
  next: string | null;
};

export type FlowNode =
  | DelayNode
  | EmailNode
  | SmsNode
  | ConditionNode
  | UpdateProfileNode
  | ListActionNode
  | WebhookNode;

export type FlowNodeType = FlowNode["type"];

export type Reentry = "never" | "always" | { after_days: number };

export type FlowSettings = {
  reentry?: Reentry;
  exit_condition?: SegmentDefinition;
  profile_filter?: SegmentDefinition;
  smart_sending_hours?: number;
};

export type FlowDefinition = { entry: string | null; nodes: FlowNode[]; settings?: FlowSettings };

export type FlowStatus = "draft" | "live" | "paused";

export type Flow = {
  id: string;
  name: string;
  status: FlowStatus;
  trigger: FlowTrigger;
  definition: FlowDefinition;
  created_at: string;
  updated_at: string;
  stats: MessageStats & { active_runs: number; total_runs: number };
};

export type FlowReport = {
  stats: MessageStats;
  nodes: { node_id: string; entered: number; stats: MessageStats }[];
};

export type FlowBlueprint = {
  key: string;
  name: string;
  description: string;
  // The contract leaves this untyped; the backend may send a label or a list of channels.
  channel_mix: string | string[];
  trigger: FlowTrigger;
  definition: FlowDefinition;
};

// ---- Audience ----

export type List = {
  id: string;
  name: string;
  double_opt_in: boolean;
  member_count: number;
  created_at: string;
};

export type Segment = {
  id: string;
  name: string;
  definition: SegmentDefinition;
  member_count: number;
  refreshed_at: string | null;
  created_at: string;
  updated_at: string;
};

export type ProfileMessage = {
  id: string;
  channel: Channel;
  subject: string | null;
  status: string;
  campaign_id: string | null;
  flow_id: string | null;
  sent_at: string | null;
  opened_at: string | null;
  clicked_at: string | null;
};

export type ProfileDetail = {
  profile: Profile;
  lists: List[];
  segments: Segment[];
  events: EventRow[];
  messages: ProfileMessage[];
};

export type ProfileInput = Partial<
  Pick<
    Profile,
    | "email"
    | "phone_number"
    | "external_id"
    | "first_name"
    | "last_name"
    | "properties"
    | "email_consent"
    | "sms_consent"
  >
> & { list_ids?: string[] };

export type ImportResult = { created: number; updated: number; skipped: number; errors: string[] };

// ---- Templates ----

export type Template = {
  id: string;
  name: string;
  channel: Channel;
  content: EmailContent | SmsContent;
  created_at: string;
  updated_at: string;
};

export type EmailRender = { subject: string; preheader: string; html: string; text: string };
export type SmsRender = { body: string; segments: number; characters: number };

// ---- Forms ----

export type FormKind = "popup" | "embed" | "flyout" | "page";
export type FormField = "email" | "phone_number" | "first_name";

export type FormConfig = {
  headline: string;
  body: string;
  button_text: string;
  success_message: string;
  collect: FormField[];
  sms_consent_text?: string;
  delay_seconds?: number;
  brand_color?: string;
  image_url?: string;
};

export type Form = {
  id: string;
  name: string;
  status: "draft" | "live";
  kind: FormKind;
  list_id: string | null;
  config: FormConfig;
  created_at: string;
  updated_at: string;
  stats: { views: number; submits: number };
};

// ---- Events and metrics ----

export type MetricSummary = { id: string; name: string; count_30d: number; last_seen: string | null };

export type EventRow = {
  id: string;
  metric: string;
  profile_id: string;
  properties: Record<string, unknown>;
  value: number | null;
  occurred_at: string;
};

export type Measurement = "count" | "unique_profiles" | "sum_value";
export type Interval = "day" | "week" | "month";

export type MetricQuery = {
  metric: string;
  measurement: Measurement;
  interval: Interval;
  start?: string;
  end?: string;
  group_by?: string;
};

export type MetricQueryResult = { series: { date: string; value: number; group?: string }[]; total: number };

// ---- Analytics ----

export type AnalyticsOverview = {
  totals: {
    revenue: number;
    attributed_revenue: number;
    campaign_revenue: number;
    flow_revenue: number;
    emails_sent: number;
    sms_sent: number;
    open_rate: number;
    click_rate: number;
    unsubscribe_rate: number;
    bounce_rate: number;
    profiles: number;
    new_profiles: number;
  };
  series: {
    date: string;
    revenue: number;
    attributed_revenue: number;
    emails_sent: number;
    sms_sent: number;
    opens: number;
    clicks: number;
  }[];
  top_campaigns: {
    id: string;
    name: string;
    sent_at: string | null;
    recipients: number;
    open_rate: number;
    click_rate: number;
    revenue: number;
  }[];
  top_flows: { id: string; name: string; status: FlowStatus; recipients: number; revenue: number }[];
};

// ---- Sending, keys, billing ----

export type DnsStatus = "not_started" | "pending" | "verified" | "failed";

export type DnsRecord = { type: string; name: string; value: string; status: string };

export type SendingStatus = {
  email: { provider: string; domain: string | null; status: DnsStatus; dns_records: DnsRecord[] };
  sms: { provider: string; from_number: string | null; status: string };
};

export type ApiKey = { id: string; name: string; prefix: string; created_at: string; last_used_at: string | null };
export type CreatedApiKey = { id: string; name: string; prefix: string; key: string };

export type Billing = {
  plan: Plan;
  limits: { profiles: number | null; emails_per_month: number | null; sms_per_month: number | null };
  usage: { profiles: number; emails_this_month: number; sms_this_month: number };
};
