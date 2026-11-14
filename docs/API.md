# Postlark HTTP API (v0.1 contract)

All JSON. Base path `/api`. IDs are UUID strings. Times are RFC 3339 UTC strings; the UI shows them in the account time zone (default `America/Los_Angeles`).

Auth:
- Browser UI: session cookie `pl_session` (HttpOnly, set by signup/login). Requests from the UI use `credentials: "include"`.
- API and MCP: `Authorization: Bearer pl_sk_...` (private API key).

Errors: non-2xx with `{ "error": { "code": "not_found" | "bad_request" | "unauthorized" | "forbidden" | "conflict" | "limit_reached" | "internal", "message": "human text" } }`.

Lists: `?limit=50&offset=0` → `{ "data": [...], "total": 123 }`.

## Shared types

```ts
type Consent = "subscribed" | "unsubscribed" | "never_subscribed";

type Profile = {
  id: string; email: string | null; phone_number: string | null; external_id: string | null;
  first_name: string | null; last_name: string | null;
  properties: Record<string, unknown>;
  email_consent: Consent; sms_consent: Consent;
  created_at: string; updated_at: string; last_event_at: string | null;
};

type Account = {
  id: string; public_key: string; name: string; website: string | null;
  from_name: string | null; from_email: string | null; reply_to: string | null;
  sms_from: string | null; timezone: string; address: string | null;
  brand_color: string; logo_url: string | null; plan: "free" | "pro" | "scale" | "self_hosted";
  onboarding: { brand: boolean; sender: boolean; contacts: boolean; first_flow: boolean; dismissed: boolean };
  created_at: string;
};

type User = { id: string; email: string; name: string | null };

// Segment definitions (also used for flow splits, flow filters and form/campaign rules)
type SegmentDefinition = { match: "all" | "any"; conditions: Condition[] };
type Condition =
  | { type: "property"; field: string; op: PropOp; value?: unknown }
    // field: "email" | "phone_number" | "first_name" | "last_name" | "external_id" | "created_at"
    //        | any custom property key, e.g. "city" or "vip"
  | { type: "event"; metric: string; op: "at_least" | "at_most" | "exactly" | "zero"; count?: number;
      within_days?: number; where?: { field: string; op: PropOp; value?: unknown }[] } // where.field: "value" or an event property key
  | { type: "list"; list_id: string; op: "in" | "not_in" }
  | { type: "consent"; channel: "email" | "sms"; status: Consent }
  | { type: "group"; match: "all" | "any"; conditions: Condition[] };
type PropOp = "eq" | "neq" | "contains" | "not_contains" | "starts_with" | "exists" | "not_exists"
  | "gt" | "gte" | "lt" | "lte" | "in" | "within_last_days" | "more_than_days_ago";

type Audience = { include: AudienceRef[]; exclude: AudienceRef[] };
type AudienceRef = { type: "list" | "segment"; id: string };

// Email content is a list of blocks rendered server-side into responsive email HTML.
type EmailBlock =
  | { type: "heading"; text: string; level?: 1 | 2 | 3; align?: "left" | "center" | "right" }
  | { type: "text"; html: string; align?: "left" | "center" | "right" } // inline HTML: p, b, strong, i, em, a, br, ul, ol, li
  | { type: "image"; src: string; alt?: string; href?: string; width?: number }
  | { type: "button"; text: string; href: string; color?: string; align?: "left" | "center" | "right" }
  | { type: "divider" }
  | { type: "spacer"; height?: number }
  | { type: "columns"; columns: EmailBlock[][] }
  | { type: "product"; title: string; image?: string; price?: string; href: string; button_text?: string }
  | { type: "html"; html: string };
type EmailContent = {
  subject: string; preheader?: string; from_name?: string; from_email?: string; reply_to?: string;
  blocks?: EmailBlock[]; html?: string; // blocks win when both are set
  theme?: { brand_color?: string; background?: string; font?: string };
};
type SmsContent = { body: string; media_url?: string };
// Template variables (minijinja / Jinja syntax) work in subject, text, button labels and SMS bodies:
// {{ first_name }}, {{ first_name|default("there") }}, {{ person.email }}, {{ person.properties.city }},
// {{ event.properties.items[0].name }} (in flows), {{ organization.name }}, {{ unsubscribe_url }}.

type Campaign = {
  id: string; name: string; channel: "email" | "sms";
  status: "draft" | "scheduled" | "sending" | "sent" | "cancelled" | "failed";
  audience: Audience; content: EmailContent | SmsContent;
  send_at: string | null; sent_at: string | null; created_at: string; updated_at: string;
  stats: MessageStats;
};
type MessageStats = {
  recipients: number; delivered: number; bounced: number; failed: number;
  unique_opens: number; unique_clicks: number; unsubscribes: number; complaints: number;
  open_rate: number; click_rate: number; // 0..1, over delivered
  orders: number; revenue: number;
};

type FlowTrigger =
  | { type: "metric"; metric: string; filters?: { field: string; op: PropOp; value?: unknown }[] }
  | { type: "list"; list_id: string }
  | { type: "segment"; segment_id: string }
  | { type: "date_property"; field: string; offset_days?: number }; // e.g. birthday, fires yearly
type FlowNode =
  | { id: string; type: "delay"; amount: number; unit: "minutes" | "hours" | "days"; next: string | null }
  | { id: string; type: "email"; name?: string; content: EmailContent; next: string | null }
  | { id: string; type: "sms"; name?: string; content: SmsContent; next: string | null }
  | { id: string; type: "condition"; condition: SegmentDefinition; yes: string | null; no: string | null }
  | { id: string; type: "update_profile"; properties: Record<string, unknown>; next: string | null }
  | { id: string; type: "add_to_list" | "remove_from_list"; list_id: string; next: string | null }
  | { id: string; type: "webhook"; url: string; method?: "POST" | "GET"; body?: unknown; next: string | null };
type FlowDefinition = {
  entry: string | null; nodes: FlowNode[];
  settings?: { reentry?: "never" | "always" | { after_days: number }; exit_condition?: SegmentDefinition; profile_filter?: SegmentDefinition; smart_sending_hours?: number };
};
type Flow = {
  id: string; name: string; status: "draft" | "live" | "paused";
  trigger: FlowTrigger; definition: FlowDefinition;
  created_at: string; updated_at: string;
  stats: MessageStats & { active_runs: number; total_runs: number };
};

type List = { id: string; name: string; double_opt_in: boolean; member_count: number; created_at: string };
type Segment = { id: string; name: string; definition: SegmentDefinition; member_count: number; refreshed_at: string | null; created_at: string; updated_at: string };
type Template = { id: string; name: string; channel: "email" | "sms"; content: EmailContent | SmsContent; created_at: string; updated_at: string };
type Form = {
  id: string; name: string; status: "draft" | "live"; kind: "popup" | "embed" | "flyout" | "page";
  list_id: string | null;
  config: { headline: string; body: string; button_text: string; success_message: string;
            collect: ("email" | "phone_number" | "first_name")[]; sms_consent_text?: string;
            delay_seconds?: number; brand_color?: string; image_url?: string };
  created_at: string; updated_at: string; stats: { views: number; submits: number };
};
type MetricSummary = { id: string; name: string; count_30d: number; last_seen: string | null };
type EventRow = { id: string; metric: string; profile_id: string; properties: Record<string, unknown>; value: number | null; occurred_at: string };
```

## Auth and account

| Method | Path | Body | Returns |
| --- | --- | --- | --- |
| POST | `/api/auth/signup` | `{email, password, name?, company}` | `{user, account}` + cookie |
| POST | `/api/auth/login` | `{email, password}` | `{user, account}` + cookie |
| POST | `/api/auth/logout` | – | `{ok: true}` |
| GET | `/api/me` | – | `{user, account}` (401 if signed out) |
| PATCH | `/api/account` | any `Account` editable fields, plus `onboarding` partial | `Account` |
| GET | `/api/account/sending` | – | `{email: {provider, domain, status: "not_started"\|"pending"\|"verified"\|"failed", dns_records: {type, name, value, status}[]}, sms: {provider, from_number, status}}` |
| POST | `/api/account/domain` | `{domain}` | same as GET sending |
| POST | `/api/account/domain/verify` | – | same as GET sending |
| GET | `/api/api-keys` | – | `{data: {id, name, prefix, created_at, last_used_at}[]}` |
| POST | `/api/api-keys` | `{name}` | `{id, name, prefix, key}` (key shown once) |
| DELETE | `/api/api-keys/:id` | – | `{ok: true}` |
| GET | `/api/billing` | – | `{plan, limits: {profiles, emails_per_month, sms_per_month}, usage: {profiles, emails_this_month, sms_this_month}}` |

## Audience

| Method | Path | Body / query | Returns |
| --- | --- | --- | --- |
| GET | `/api/profiles` | `?q=&list_id=&segment_id=&limit=&offset=` | `{data: Profile[], total}` |
| POST | `/api/profiles` | Profile fields (upsert on email, phone_number or external_id) + optional `list_ids` | `Profile` |
| GET | `/api/profiles/:id` | – | `{profile, lists: List[], segments: Segment[], events: EventRow[], messages: {id, channel, subject, status, campaign_id, flow_id, sent_at, opened_at, clicked_at}[]}` |
| PATCH | `/api/profiles/:id` | partial Profile | `Profile` |
| DELETE | `/api/profiles/:id` | – | `{ok: true}` |
| POST | `/api/profiles/import` | `{csv: string, list_id?: string, email_consent?: Consent, sms_consent?: Consent}` | `{created, updated, skipped, errors: string[]}` |
| GET | `/api/lists` | – | `{data: List[], total}` |
| POST | `/api/lists` | `{name, double_opt_in?}` | `List` |
| GET/PATCH/DELETE | `/api/lists/:id` | `{name?}` | `List` |
| POST | `/api/lists/:id/members` | `{profile_ids?: string[], emails?: string[]}` | `{added}` |
| DELETE | `/api/lists/:id/members` | `{profile_ids: string[]}` | `{removed}` |
| GET | `/api/segments` | – | `{data: Segment[], total}` |
| POST | `/api/segments` | `{name, definition}` | `Segment` |
| GET/PATCH/DELETE | `/api/segments/:id` | `{name?, definition?}` | `Segment` |
| POST | `/api/segments/preview` | `{definition}` | `{count, sample: Profile[]}` |

## Events and metrics

| Method | Path | Body / query | Returns |
| --- | --- | --- | --- |
| POST | `/api/events` | `{metric, profile: {email?, phone_number?, external_id?, id?, first_name?, ...}, properties?, value?, time?, unique_id?}` | `{id, profile_id}` |
| GET | `/api/events` | `?metric=&profile_id=&limit=&offset=` | `{data: EventRow[], total}` |
| GET | `/api/metrics` | – | `{data: MetricSummary[]}` |
| POST | `/api/metrics/query` | `{metric, measurement: "count"\|"unique_profiles"\|"sum_value", interval: "day"\|"week"\|"month", start?, end?, group_by?: string}` | `{series: {date, value, group?}[], total}` |

## Messaging

| Method | Path | Body | Returns |
| --- | --- | --- | --- |
| GET | `/api/templates` | `?channel=` | `{data: Template[], total}` |
| POST | `/api/templates` | `{name, channel, content}` | `Template` |
| GET/PATCH/DELETE | `/api/templates/:id` | partial | `Template` |
| POST | `/api/render` | `{channel, content, profile_id?}` | email: `{subject, preheader, html, text}`; sms: `{body, segments, characters}` |
| GET | `/api/campaigns` | `?status=&channel=` | `{data: Campaign[], total}` |
| POST | `/api/campaigns` | `{name, channel, audience?, content?}` | `Campaign` (draft) |
| GET/PATCH/DELETE | `/api/campaigns/:id` | partial (draft/scheduled only) | `Campaign` |
| POST | `/api/campaigns/estimate` | `{channel, audience}` | `{count}` (reachable, consented recipients) |
| POST | `/api/campaigns/:id/test` | `{to: string[]}` (emails or phone numbers) | `{sent}` |
| POST | `/api/campaigns/:id/schedule` | `{send_at}` | `Campaign` |
| POST | `/api/campaigns/:id/send` | – | `Campaign` (status `sending`) |
| POST | `/api/campaigns/:id/cancel` | – | `Campaign` |
| POST | `/api/campaigns/:id/duplicate` | – | `Campaign` |
| GET | `/api/campaigns/:id/report` | – | `{stats: MessageStats, links: {url, clicks}[], series: {hour, opens, clicks}[]}` |
| GET | `/api/flows` | – | `{data: Flow[], total}` |
| POST | `/api/flows` | `{name, trigger, definition}` | `Flow` (draft) |
| GET/PATCH/DELETE | `/api/flows/:id` | partial | `Flow` |
| POST | `/api/flows/:id/status` | `{status: "live"\|"paused"\|"draft"}` | `Flow` |
| GET | `/api/flows/:id/report` | – | `{stats, nodes: {node_id, entered, stats: MessageStats}[]}` |
| GET | `/api/flows/blueprints` | – | `{data: {key, name, description, channel_mix, trigger, definition}[]}` |
| POST | `/api/flows/from-blueprint` | `{key, name?}` | `Flow` |
| GET | `/api/forms` | – | `{data: Form[], total}` |
| POST | `/api/forms` | `{name, kind, list_id, config}` | `Form` |
| GET/PATCH/DELETE | `/api/forms/:id` | partial | `Form` |

## Analytics

| Method | Path | Query | Returns |
| --- | --- | --- | --- |
| GET | `/api/analytics/overview` | `?days=30` | `{totals: {revenue, attributed_revenue, campaign_revenue, flow_revenue, emails_sent, sms_sent, open_rate, click_rate, unsubscribe_rate, bounce_rate, profiles, new_profiles}, series: {date, revenue, attributed_revenue, emails_sent, sms_sent, opens, clicks}[], top_campaigns: {id, name, sent_at, recipients, open_rate, click_rate, revenue}[], top_flows: {id, name, status, recipients, revenue}[]}` |

Attribution: a conversion event (`Placed Order` by default) is credited to the newest email the person opened or clicked in the previous 5 days, or the newest SMS they clicked in the previous 24 hours.

## Public endpoints (no auth)

- `POST /client/events?k=<public_key>` – browser tracking (`{metric, profile, properties}`), CORS open.
- `POST /client/subscribe?k=<public_key>` – form submit (`{form_id, email?, phone_number?, first_name?, sms_consent?}`).
- `GET /f/:form_id` hosted sign-up page, `GET /f/:form_id/embed.js` embed script.
- `GET /t/o/:token` open pixel, `GET /t/c/:token?u=<url>` click redirect, `GET|POST /u/:token` unsubscribe (RFC 8058 one-click).
- `POST /webhooks/ses/:secret`, `POST /webhooks/telnyx/:secret`, `POST /webhooks/twilio/:secret` – delivery, bounce, complaint and inbound STOP.
- `POST /mcp` – Model Context Protocol (Streamable HTTP), `Authorization: Bearer pl_sk_...`.
- `GET /healthz`.
