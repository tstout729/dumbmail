// Types mirror docs/API.md (v0.1 contract plus the "v0.2 additions" section). Keep them in sync with that file.

/** `pending`: signed up through a double opt-in form and hasn't confirmed by email yet (gets no email). */
export type Consent = "subscribed" | "unsubscribed" | "never_subscribed" | "pending";

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
  /** Network (instagram, facebook, tiktok, x, youtube, pinterest, linkedin, threads) → profile URL. */
  social_links: Partial<Record<SocialNetwork, string>>;
  plan: Plan;
  onboarding: Onboarding;
  created_at: string;
  /** Set while email sending is paused for high bounces or spam complaints; only support can lift it. */
  sending_paused_at: string | null;
  /** What tripped the pause, e.g. "hard bounce rate 12.0% (12 of 100 emails) …". */
  sending_paused_reason: string | null;
  /** Campaign email skips people who got many emails over the last months and engaged with none. */
  email_sunset_enabled: boolean;
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
    | "email_sunset_enabled"
  >
> & { onboarding?: Partial<Onboarding>; social_links?: Partial<Record<SocialNetwork, string>> };

export const socialNetworks = ["instagram", "facebook", "tiktok", "x", "youtube", "pinterest", "linkedin", "threads"] as const;
export type SocialNetwork = (typeof socialNetworks)[number];
export const socialLabels: Record<SocialNetwork, string> = {
  instagram: "Instagram",
  facebook: "Facebook",
  tiktok: "TikTok",
  x: "X",
  youtube: "YouTube",
  pinterest: "Pinterest",
  linkedin: "LinkedIn",
  threads: "Threads",
};

/** What brand import found on a website. Nothing is saved until the user confirms. */
export type BrandKit = {
  website: string;
  name: string | null;
  logo_url: string | null;
  brand_color: string | null;
  colors: string[];
  address: string | null;
  from_email: string | null;
  from_email_found: boolean;
  social_links: Partial<Record<SocialNetwork, string>>;
};

export type User = { id: string; email: string; name: string | null };

export type Session = { user: User; account: Account; support_email: string | null };

// ---- Segments and conditions ----

export type PropOp =
  | "eq"
  | "neq"
  | "contains"
  | "not_contains"
  | "starts_with"
  | "ends_with"
  | "exists"
  | "not_exists"
  | "gt"
  | "gte"
  | "lt"
  | "lte"
  | "between"
  | "in"
  | "not_in"
  | "within_last_days"
  | "more_than_days_ago"
  | "anniversary_within_days";

export type FieldFilter = { field: string; op: PropOp; value?: unknown };

export type EventCountOp = "at_least" | "at_most" | "exactly" | "zero";
export type CmpOp = "eq" | "neq" | "gt" | "gte" | "lt" | "lte" | "between";
export type TimeOp = "within_last_days" | "more_than_days_ago" | "before" | "after" | "between";
export type HasOp = "has" | "has_not";
export type Prediction =
  | "historic_clv"
  | "predicted_clv"
  | "order_count"
  | "average_order_value"
  | "days_since_last_order"
  | "average_days_between_orders"
  | "expected_next_order_in_days"
  | "churn_risk";

/** Time window for event rules. since/until are YYYY-MM-DD days in the account's time zone. */
export type EventWindow = { within_days?: number; since?: string; until?: string };

export type PropertyCondition = { type: "property"; field: string; op: PropOp; value?: unknown };
export type EventCondition = {
  type: "event";
  metric: string;
  op: EventCountOp;
  count?: number;
  where?: FieldFilter[];
} & EventWindow;
export type EventValueCondition = {
  type: "event_value";
  metric: string;
  aggregate: "sum" | "average" | "min" | "max";
  field?: string;
  op: CmpOp;
  value: number | [number, number];
  where?: FieldFilter[];
} & EventWindow;
export type EventTimeCondition = {
  type: "event_time";
  metric: string;
  which: "first" | "last";
  op: TimeOp;
  value: number | string | [string, string];
  where?: FieldFilter[];
};
export type ListCondition = { type: "list"; list_id: string; op: "in" | "not_in" };
export type SegmentRefCondition = { type: "segment"; segment_id: string; op: "in" | "not_in" };
export type ConsentCondition = { type: "consent"; channel: "email" | "sms"; status: Consent };
export type CanReceiveCondition = { type: "can_receive"; channel: "email" | "sms"; value: boolean };
export type SuppressedCondition = { type: "suppressed"; value: boolean };
export type FormCondition = { type: "form"; form_id?: string; op: HasOp } & EventWindow;
export type EngagementCondition = { type: "engagement"; channel: "email" | "sms" | "any"; engaged: boolean; within_days: number };
export type MessageCondition = {
  type: "message";
  action: "received" | "opened" | "clicked";
  campaign_id?: string;
  flow_id?: string;
  op: HasOp;
  within_days?: number;
};
export type PredictionCondition = {
  type: "prediction";
  measure: Prediction;
  op: CmpOp | "in";
  value: number | [number, number] | string | string[];
  metric?: string;
};
export type RandomCondition = { type: "random"; percent: number; seed?: string };
export type GroupCondition = { type: "group"; match: "all" | "any"; conditions: Condition[] };

export type Condition =
  | PropertyCondition
  | EventCondition
  | EventValueCondition
  | EventTimeCondition
  | ListCondition
  | SegmentRefCondition
  | ConsentCondition
  | CanReceiveCondition
  | SuppressedCondition
  | FormCondition
  | EngagementCondition
  | MessageCondition
  | PredictionCondition
  | RandomCondition
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

export type CampaignStatus = "draft" | "scheduled" | "sending" | "testing" | "sent" | "cancelled" | "failed";

/** A/B test version ids. "a" is the campaign's own content; b-d are `ab_test.variants` in order. */
export type VersionId = "a" | "b" | "c" | "d";
export type AbMetric = "open_rate" | "click_rate" | "revenue";

export type AbTest = {
  variants: { id: Exclude<VersionId, "a">; content: EmailContent | SmsContent }[];
  /** Share of the audience that gets the test, split evenly across versions (5-100, default 20). */
  test_percent?: number;
  /** Hours to wait before picking the winner (1-168, default 4). */
  wait_hours?: number;
  /** open_rate is email only. Default click_rate. */
  metric?: AbMetric;
  /** auto sends the leader when the wait ends; manual waits for someone to pick. Default auto. */
  pick?: "auto" | "manual";
};

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
  ab_test: AbTest | null;
  /** When the test slice's wait ends (set once the test slice has gone out). */
  ab_test_ends_at: string | null;
  ab_winner: VersionId | null;
};

export type CampaignAbReport = {
  metric: AbMetric;
  pick: "auto" | "manual";
  test_percent: number;
  wait_hours: number;
  ends_at: string | null;
  winner: VersionId | null;
  leader: VersionId;
  /** Probability (0.5-1) that the leader really beats the runner-up; null for revenue. */
  confidence: number | null;
  versions: { id: VersionId; stats: MessageStats }[];
};

export type CampaignReport = {
  ab_test: CampaignAbReport | null;
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
/** Sends each person down `yes` (path A) with `percent`% chance, otherwise `no` (path B). */
export type RandomSplitNode = {
  id: string;
  type: "random_split";
  percent: number;
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
  | RandomSplitNode
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
  /** Listed on the preference page people reach from an email's unsubscribe link. */
  show_on_preferences: boolean;
  /** Shown under the name on the preference page. */
  description: string;
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
// Mirrors docs/FORMS.md. Every config key is optional (left out = the default); `null` resets a key on update.

export type FormKind = "popup" | "embed" | "flyout" | "page";
/** The older `collect` field list. Forms without `fields` derive their fields from it. */
export type FormField = "email" | "phone_number" | "first_name";
export type StandardFieldKey = "email" | "phone_number" | "first_name" | "last_name" | "birthday";
export type CustomFieldType = "text" | "number" | "date" | "select" | "radio" | "checkboxes" | "checkbox";

export type FormFieldDef = {
  /** A standard key, or a custom profile property (lowercase letters, digits and _, starting with a letter, ≤ 40). */
  key: string;
  /** Custom fields only; default "text". */
  type?: CustomFieldType;
  label?: string;
  placeholder?: string;
  /** Default true for email, false for the rest. */
  required?: boolean;
  /** Required for select, radio and checkboxes: 1-20 options. */
  options?: string[];
};

export type PhoneStep = {
  enabled?: boolean;
  headline?: string;
  body?: string;
  button_text?: string;
  skip_text?: string;
};

export type FormLayout = "stacked" | "image_left" | "image_right" | "background";
export type FormAlign = "center" | "left";
export type FormFont =
  | "system"
  | "inherit"
  | "serif"
  | "Inter"
  | "Poppins"
  | "Montserrat"
  | "Lato"
  | "DM Sans"
  | "Space Grotesk"
  | "Playfair Display"
  | "Lora"
  | "Merriweather"
  | "Oswald";
export type FormShadow = "none" | "soft" | "strong";
export type FormAnimation = "fade" | "slide" | "zoom" | "none";
export type PopupPosition = "center" | "top" | "bottom";
export type FlyoutPosition = "bottom_right" | "bottom_left" | "top_right" | "top_left";
export type HeadlineWeight = 400 | 500 | 600 | 700 | 800;

/** One look shared by every A/B version. Sent whole on update. */
export type FormStyle = {
  layout?: FormLayout;
  width?: number;
  align?: FormAlign;
  font?: FormFont;
  headline_size?: number;
  headline_weight?: HeadlineWeight;
  body_size?: number;
  background_color?: string;
  headline_color?: string;
  text_color?: string;
  button_color?: string;
  button_text_color?: string;
  button_full_width?: boolean;
  input_background?: string;
  input_border_color?: string;
  input_text_color?: string;
  corner_radius?: number;
  button_radius?: number;
  input_radius?: number;
  padding?: number;
  border_width?: number;
  border_color?: string;
  shadow?: FormShadow;
  overlay_color?: string;
  overlay_opacity?: number;
  close_on_overlay?: boolean;
  close_color?: string;
  position?: PopupPosition | FlyoutPosition;
  animation?: FormAnimation;
  page_background?: string;
  custom_css?: string;
};

export type TeaserPosition = "bottom_left" | "bottom_right" | "left" | "right";
export type TeaserShow = "after_close" | "always";
export type Teaser = { enabled?: boolean; text?: string; position?: TeaserPosition; show?: TeaserShow };

export type FormConfig = {
  // Content (also per A/B version)
  headline?: string | null;
  body?: string | null;
  button_text?: string | null;
  image_url?: string | null;
  image_alt?: string | null;
  no_thanks_text?: string | null;
  success_message?: string | null;
  success_body?: string | null;
  discount_code?: string | null;
  // Shared by every version
  legal_text?: string | null;
  success_button_text?: string | null;
  success_button_url?: string | null;
  redirect_url?: string | null;
  auto_close_seconds?: number | null;
  confirm_message?: string | null;
  sms_consent_text?: string | null;
  brand_color?: string | null;
  fields?: FormFieldDef[] | null;
  /** Legacy field list; used only while `fields` is absent. */
  collect?: FormField[] | null;
  phone_step?: PhoneStep | null;
  style?: FormStyle | null;
  teaser?: Teaser | null;
  delay_seconds?: number | null;
  targeting?: Targeting | null;
  /** A/B test: versions b-d in order override the per-version keys; the form itself is version A. */
  variants?: FormVariant[] | null;
};

export type FormVariantField =
  | "headline"
  | "body"
  | "button_text"
  | "image_url"
  | "image_alt"
  | "no_thanks_text"
  | "success_message"
  | "success_body"
  | "discount_code";
export type FormVariant = { id: Exclude<VersionId, "a"> } & Partial<Record<FormVariantField, string | null>>;

export type FormTrigger = "delay" | "scroll" | "exit_intent" | "manual";
export type FormDevices = "all" | "desktop" | "mobile";
export type FormVisitors = "all" | "new" | "returning";

/** Popup and flyout display rules. Every field is optional; defaults are noted. Sent whole on update. */
export type Targeting = {
  trigger?: FormTrigger; // default "delay" (uses config.delay_seconds)
  scroll_percent?: number; // for "scroll", default 40
  devices?: FormDevices; // default "all"
  url_contains?: string[];
  url_excludes?: string[];
  page_views?: number; // default 1
  visitors?: FormVisitors; // default "all"
  frequency_days?: number; // default 7
  hide_for_subscribers?: boolean; // default true
  start_at?: string; // ISO 8601
  end_at?: string;
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
  /** The hosted page, drafts included; nothing is counted or saved there. */
  preview_url: string;
  stats: { views: number; submits: number };
  /** Per-version counts while an A/B test runs (empty otherwise). */
  variant_stats: { id: VersionId; views: number; submits: number; submit_rate: number }[];
};

/** An uploaded image (`POST /api/images`). The URL is public and permanent. */
export type UploadedImage = { id: string; url: string; content_type: string; size: number; created_at: string };

// ---- Events and metrics ----

export type MetricSummary = {
  id: string;
  name: string;
  count_30d: number;
  count_prev_30d: number;
  last_seen: string | null;
  /** Whether any event of this metric carries a value (e.g. an order total). */
  has_value: boolean;
  /** 30 daily counts, oldest first. */
  trend: number[];
};

export type EventRow = {
  id: string;
  metric: string;
  profile_id: string;
  properties: Record<string, unknown>;
  value: number | null;
  occurred_at: string;
  /** Email, phone or name; only on the events list. */
  profile_label?: string | null;
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

export type OverviewTotals = {
  revenue: number;
  attributed_revenue: number;
  campaign_revenue: number;
  flow_revenue: number;
  orders: number;
  attributed_orders: number;
  emails_sent: number;
  sms_sent: number;
  emails_delivered: number;
  opens: number;
  clicks: number;
  email_clicks: number;
  open_rate: number;
  click_rate: number;
  unsubscribe_rate: number;
  bounce_rate: number;
  complaint_rate: number;
  new_profiles: number;
};

export type AnalyticsOverview = {
  days: number;
  totals: OverviewTotals & { profiles: number };
  /** The same totals for the `days` before this range. */
  previous: OverviewTotals;
  series: {
    date: string;
    revenue: number;
    attributed_revenue: number;
    campaign_revenue: number;
    flow_revenue: number;
    emails_sent: number;
    sms_sent: number;
    opens: number;
    clicks: number;
    new_profiles: number;
  }[];
  top_campaigns: {
    id: string;
    name: string;
    sent_at: string | null;
    recipients: number;
    open_rate: number;
    click_rate: number;
    orders: number;
    revenue: number;
  }[];
  /** Flow numbers cover the selected range only. */
  top_flows: {
    id: string;
    name: string;
    status: FlowStatus;
    recipients: number;
    open_rate: number;
    click_rate: number;
    orders: number;
    revenue: number;
  }[];
};

export type FlowTotals = {
  revenue: number;
  orders: number;
  /** All tracked store revenue in the range, so flow revenue can be shown as a share. */
  store_revenue: number;
  messages_sent: number;
  recipients: number;
  delivered: number;
  opens: number;
  clicks: number;
  open_rate: number;
  click_rate: number;
  revenue_per_recipient: number;
};

export type FlowAnalytics = {
  days: number;
  totals: FlowTotals;
  /** The same totals for the `days` before this range. */
  previous: FlowTotals;
  series: { date: string; revenue: number; messages_sent: number }[];
};

// ---- Sending and domains ----

export type DnsStatus = "not_started" | "pending" | "verified" | "failed";
export type LinkDomainStatus = "not_started" | "pending" | "active" | "failed";

export type DnsPurpose = "dkim" | "spf" | "mail_from" | "dmarc" | "link" | "verification";
export type DnsRecordStatus = "verified" | "pending" | "missing" | "recommended";

export type DnsRecord = {
  type: "CNAME" | "TXT" | "MX";
  name: string;
  value: string;
  priority?: number;
  purpose: DnsPurpose;
  status: DnsRecordStatus;
  found?: string | null; // what public DNS returned
};

export type SendingDomain = {
  domain: string;
  status: DnsStatus;
  mail_from_domain: string | null;
  dns_records: DnsRecord[];
  created_at: string;
  verified_at: string | null;
  /** The account's default sender email is on this domain. */
  default_sender: boolean;
};

export type Sending = {
  email: {
    provider: string;
    /** The main domain (the default sender's, else the first verified); see `domains` for all of them. */
    domain: string | null;
    from_email: string | null;
    /** The default sender email is on a verified domain. */
    from_email_verified: boolean;
    status: DnsStatus;
    mail_from_domain: string | null;
    dns_records: DnsRecord[];
    domains: SendingDomain[];
    max_domains: number;
  };
  sms: { provider: string; from_number: string | null; status: string };
  link_domain: { domain: string | null; status: LinkDomainStatus; dns_records: DnsRecord[]; url: string | null };
  /** Hosted service only: the 24-hour email limit, which starts small and grows while bounces and complaints stay low. */
  email_limit?: { limit: number; sent_last_24h: number; remaining: number; fixed: boolean; start: number };
  unengaged_skip: { enabled: boolean; days: number; min_emails: number };
};

// ---- API keys ----

export type ApiKey = { id: string; name: string; prefix: string; created_at: string; last_used_at: string | null };
export type CreatedApiKey = { id: string; name: string; prefix: string; key: string };

// ---- Billing (v0.2: prepaid credit) ----

export type UsageKind = "email" | "sms" | "mms" | "number";

export type BillingPrices = {
  email_per_1000: number;
  sms_per_segment: number;
  mms_per_message: number;
  phone_number_monthly: number;
};

export type AutoRecharge = { enabled: boolean; threshold: number; amount: number };

export type LedgerEntry = { id: string; kind: string; description: string; amount: number; created_at: string };

/** "renewing": the month ended and the renewal charge hasn't settled yet. */
export type PlanStatus = "none" | "active" | "cancelling" | "renewing" | "renewal_failed";

/** The monthly email plan. Fields past `status` are present whenever there is a plan (status ≠ "none"). */
export type EmailPlan =
  | { status: "none"; ended_at: string | null; ended_reason: "cancelled" | "renewal_failed" | null }
  | {
      status: Exclude<PlanStatus, "none">;
      tier: number;
      price: number;
      emails_included: number;
      emails_used: number;
      emails_remaining: number;
      period_start: string;
      period_end: string;
      /** A downgrade waiting for the next renewal. */
      next_tier: number | null;
      next_price: number | null;
      next_emails: number | null;
      renewal_attempts: number;
      renewal_failed_at: string | null;
      max_renewal_attempts: number;
    };

export type PlanOption = { tier: number; price: number; emails: number; price_per_1000: number; pay_as_you_go_price: number };

export type PlanPayment = { id: string; kind: "start" | "upgrade" | "renewal"; tier: number; amount: number; description: string; created_at: string };

export type Billing = {
  mode: "hosted" | "self_hosted";
  balance: number;
  free_emails_per_month: number;
  emails_this_month: number;
  free_emails_remaining: number;
  sms_this_month: number;
  prices: BillingPrices;
  auto_recharge: AutoRecharge;
  has_payment_method: boolean;
  stripe_enabled: boolean;
  usage: { date: string; kind: UsageKind; quantity: number; amount: number }[];
  ledger: LedgerEntry[];
  limits: { profiles: number | null };
  plan: EmailPlan;
  plan_options: PlanOption[];
  plan_payments: PlanPayment[];
  /** Smallest card top-up in USD. */
  min_topup: number;
};

// ---- Phone numbers and SMS ----

export type PhoneNumberType = "toll_free" | "local";
export type SmsProvider = "telnyx" | "twilio";

export type PhoneNumberStatus =
  | "active"
  | "needs_verification"
  | "verification_pending"
  | "verified"
  | "rejected"
  | "released";

export type PhoneNumber = {
  id: string;
  phone_number: string;
  type: PhoneNumberType;
  source: "purchased" | "connected";
  provider: SmsProvider;
  status: PhoneNumberStatus;
  verification_note: string | null;
  monthly_price: number;
  created_at: string;
  /** Set when this month's rent couldn't be paid from the balance: the number is released then unless credit is added. */
  release_at: string | null;
};

export type SmsSetup = {
  mode: "platform" | "byo";
  provider: string; // telnyx | twilio | log
  from_number: string | null;
  can_send: boolean;
  inbound_webhook_url: string;
  platform_available: boolean;
};

export type PhoneNumbersResponse = { data: PhoneNumber[]; sms: SmsSetup };

export type AvailableNumber = { phone_number: string; type: PhoneNumberType; region: string | null; monthly_price: number };

export type TollFreeUseCase = "Marketing" | "Mixed" | "Account Notifications" | "Customer Care" | "Delivery Notifications";
export type MonthlyVolume = "10" | "100" | "1,000" | "10,000" | "100,000";

export type TollFreeVerification = {
  business_name: string;
  website: string;
  address1: string;
  address2?: string;
  city: string;
  state: string;
  zip: string;
  contact_first_name: string;
  contact_last_name: string;
  contact_email: string;
  contact_phone: string;
  use_case: TollFreeUseCase;
  use_case_summary: string;
  sample_message: string;
  opt_in_description: string;
  opt_in_image_url: string;
  monthly_volume: MonthlyVolume;
};

export type SmsConnectInput =
  | { provider: "telnyx"; api_key: string; phone_number: string; messaging_profile_id?: string }
  | { provider: "twilio"; account_sid: string; auth_token: string; phone_number: string };

// ---- Website snippet ----

export type WebsiteStatus = {
  snippet: string;
  /** Code for Shopify's Customer events → custom pixel (checkout and order tracking). */
  shopify_pixel: string;
  public_key: string;
  installed: boolean;
  last_seen_at: string | null;
  last_seen_url: string | null;
};

export type SitePlatform = "shopify" | "wordpress" | "squarespace" | "wix" | "webflow" | "framer" | "bigcommerce" | "nextjs";

export type WebsiteCheck = { found: boolean; message: string; platform: SitePlatform | null };

/** One thing the website script reported during a setup test. */
export type SiteTestCheck = {
  at: string;
  ok?: boolean;
  summary?: string;
  message?: string;
  platform?: string;
  tracked_by?: string;
  email?: string | null;
  key?: string;
  count?: number;
};

export type SiteTestCheckKey =
  | "loaded"
  | "forms"
  | "product"
  | "cart"
  | "checkout"
  | "order"
  | "identify"
  | "pixel"
  | "custom"
  | "wrong_key"
  | "warn_duplicate"
  | "warn_csp"
  | "warn_key"
  | "warn_network";

export type SiteTest = {
  code: string;
  url: string | null;
  open_url: string | null;
  checks: Partial<Record<SiteTestCheckKey, SiteTestCheck>>;
  created_at: string;
  expires_at: string;
  active: boolean;
  last_seen_at: string | null;
  last_url: string | null;
};
