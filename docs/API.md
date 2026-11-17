# DumbMail HTTP API (v0.1 contract)

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
  // Set while email sending is paused for high bounces or spam complaints (see "Automatic email pause").
  sending_paused_at: string | null; sending_paused_reason: string | null;
  // Campaign email skips people who never engage (see "Sending guardrails"). Editable with PATCH /api/account.
  email_sunset_enabled: boolean;
};

type User = { id: string; email: string; name: string | null };

// Segment definitions (also used for flow splits, flow filters and form/campaign rules)
type SegmentDefinition = { match: "all" | "any"; conditions: Condition[] };
// Dates written YYYY-MM-DD are whole days in the account's time zone. The MCP resource
// dumbmail://docs/segments explains every rule with examples.
type Window = { within_days?: number; since?: string; until?: string }; // since/until: YYYY-MM-DD, both days included
type EventFilter = { field: string; op: PropOp; value?: unknown }; // field: "value" or an event property key
type Condition =
  | { type: "property"; field: string; op: PropOp; value?: unknown }
    // field: "email" | "phone_number" | "first_name" | "last_name" | "external_id" | "created_at" | "updated_at"
    //        | "last_event_at" | "email_consent" | "sms_consent" | "email_suppressed_reason"
    //        | any custom property key, e.g. "city" or "vip". Location: "country", "region", "city", "zip", "timezone".
  | ({ type: "event"; metric: string; op: "at_least" | "at_most" | "exactly" | "zero"; count?: number; where?: EventFilter[] } & Window)
  | ({ type: "event_value"; metric: string; aggregate: "sum" | "average" | "min" | "max"; field?: string; // default "value"
       op: CmpOp; value: number | [number, number]; where?: EventFilter[] } & Window)
  | { type: "event_time"; metric: string; which: "first" | "last";
      op: "within_last_days" | "more_than_days_ago" | "before" | "after" | "between"; value: number | string | [string, string]; where?: EventFilter[] }
  | { type: "list"; list_id: string; op: "in" | "not_in" }
  | { type: "segment"; segment_id: string; op: "in" | "not_in" } // the other segment's rules, evaluated live
  | { type: "consent"; channel: "email" | "sms"; status: Consent }
  | { type: "can_receive"; channel: "email" | "sms"; value?: boolean } // a send would reach them now
  | { type: "suppressed"; value?: boolean } // email suppressed after a hard bounce or complaint
  | ({ type: "form"; form_id?: string; op?: "has" | "has_not" } & Window) // signed up through a form (any form without form_id)
  | { type: "engagement"; channel: "email" | "sms" | "any"; engaged?: boolean; within_days: number }
  | { type: "message"; action: "received" | "opened" | "clicked"; campaign_id?: string; flow_id?: string; // exactly one of the two
      op?: "has" | "has_not"; within_days?: number }
  | { type: "prediction"; measure: Prediction; op: CmpOp | "in"; value: number | [number, number] | string | string[]; metric?: string } // default "Placed Order"
  | { type: "random"; percent: number; seed?: string } // stable share of profiles for a seed
  | { type: "group"; match: "all" | "any"; conditions: Condition[] };
type PropOp = "eq" | "neq" | "contains" | "not_contains" | "starts_with" | "ends_with" | "exists" | "not_exists"
  | "gt" | "gte" | "lt" | "lte" | "between" | "in" | "not_in" | "within_last_days" | "more_than_days_ago" | "anniversary_within_days";
type CmpOp = "eq" | "neq" | "gt" | "gte" | "lt" | "lte" | "between";
type Prediction = "historic_clv" | "predicted_clv" | "order_count" | "average_order_value" | "days_since_last_order"
  | "average_days_between_orders" | "expected_next_order_in_days" | "churn_risk"; // churn_risk: "low" | "medium" | "high" with eq, neq or in

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

type List = { id: string; name: string; double_opt_in: boolean; show_on_preferences: boolean; description: string; member_count: number; created_at: string };
type Segment = { id: string; name: string; definition: SegmentDefinition; member_count: number; refreshed_at: string | null; created_at: string; updated_at: string };
type Template = { id: string; name: string; channel: "email" | "sms"; content: EmailContent | SmsContent; created_at: string; updated_at: string };
type Form = {
  id: string; name: string; status: "draft" | "live"; kind: "popup" | "embed" | "flyout" | "page";
  list_id: string | null;
  config: FormConfig;  // every key is documented in docs/FORMS.md
  created_at: string; updated_at: string; stats: { views: number; submits: number };
  variant_stats: { id: "a" | "b" | "c" | "d"; views: number; submits: number; submit_rate: number }[];  // [] unless A/B testing
  preview_url: string;  // hosted page that shows drafts too and counts and saves nothing
};
type Image = { id: string; url: string; content_type: "image/png" | "image/jpeg" | "image/gif" | "image/webp"; size: number; created_at: string };
type MetricSummary = { id: string; name: string; count_30d: number; last_seen: string | null };
type EventRow = { id: string; metric: string; profile_id: string; properties: Record<string, unknown>; value: number | null; occurred_at: string };
```

## Auth and account

| Method | Path | Body | Returns |
| --- | --- | --- | --- |
| POST | `/api/auth/signup` | `{email, password, name?, company}` | `{user, account}` + cookie |
| POST | `/api/auth/login` | `{email, password}` | `{user, account}` + cookie |
| POST | `/api/auth/logout` | – | `{ok: true}` |
| GET | `/api/auth/options` | – | `{google, signup, site_url}`: which sign-in methods this server offers, and the marketing site's base URL when SITE_URL puts it on its own host (else `null`) |
| GET | `/api/auth/google?next=/path` | – | Redirects to Google ("Continue with Google"). The callback (`/api/auth/google/callback`) signs in, joins an existing user with the same email (turning its password off and ending older sessions), or creates a new account and sends it to `/onboarding`. Failures redirect to `/login?error=<code>`. With GOOGLE_REDIRECT_BASE on another host, Google returns to that host and the callback sends people on to PUBLIC_URL |
| GET | `/api/me` | – | `{user, account, support_email}` (401 if signed out). `support_email` is the server's SUPPORT_EMAIL or null; signup and login return it too |
| PATCH | `/api/account` | any `Account` editable fields, plus `onboarding` partial. `social_links` is `{instagram\|facebook\|tiktok\|x\|youtube\|pinterest\|linkedin\|threads: https URL}`; a blank value removes that network. Social links appear in email footers | `Account` |
| POST | `/api/brand/import` | `{url, apply?}` | `{brand: {website, name, logo_url, brand_color, colors[], address, from_email, from_email_found, social_links}}`. Reads the public website (and its stylesheets and logo) for brand suggestions. With `apply: true`, also saves website, logo, brand color and social links, plus name and address when the account has none, and returns `account` |
| GET | `/api/account/sending` | – | `{email: {provider, domain, status: "not_started"\|"pending"\|"verified"\|"failed", dns_records: {type, name, value, status}[]}, sms: {provider, from_number, status}, email_limit?: {limit, sent_last_24h, remaining, fixed, start}, unengaged_skip: {enabled, days, min_emails}}`. `email_limit` is the hosted service's 24-hour warm-up limit (see "Sending guardrails") |
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
| POST | `/api/profiles/import` | `{csv: string, list_id?: string, email_consent?: Consent, sms_consent?: Consent, permission_confirmed?: boolean}` | `{created, updated, skipped, errors: string[]}`. On the hosted service `permission_confirmed: true` (everyone in the file agreed to get email) is required unless `email_consent` is `unsubscribed`; without it the import is refused with 400. Unless importing as unsubscribed, shared inboxes (info@, sales@, no-reply@ …) and throwaway addresses (mailinator.com …) are left out and summarized at the top of `errors`; a row with nothing else to match on (phone, external id) is skipped, otherwise the rest of it imports |
| GET | `/api/lists` | – | `{data: List[], total}` |
| POST | `/api/lists` | `{name, double_opt_in?, show_on_preferences?, description?}` | `List`. With `show_on_preferences`, the list appears on the preference page people reach from an email's unsubscribe link, where they can join or leave it themselves. With `double_opt_in`, people who sign up through a form get a confirmation email and join only after confirming (see "Sending guardrails") |
| GET/PATCH/DELETE | `/api/lists/:id` | `{name?, double_opt_in?, show_on_preferences?, description?}` | `List` |
| POST | `/api/lists/:id/members` | `{profile_ids?: string[], emails?: string[]}` | `{added}` |
| DELETE | `/api/lists/:id/members` | `{profile_ids: string[]}` | `{removed}` |
| GET | `/api/segments` | – | `{data: Segment[], total}` |
| POST | `/api/segments` | `{name, definition}` | `Segment` |
| GET/PATCH/DELETE | `/api/segments/:id` | `{name?, definition?}` | `Segment`. GET recounts members live. Saving fails with 400 if the rules include the segment itself (directly or through another segment); DELETE fails with 409 while another segment or a running flow uses it. Every segment's stored membership and `member_count` refresh about every 15 minutes (every 5 for segments that trigger flows). |
| POST | `/api/segments/preview` | `{definition}` | `{count, sample: Profile[]}` |

## Events and metrics

| Method | Path | Body / query | Returns |
| --- | --- | --- | --- |
| POST | `/api/events` | `{metric, profile: {email?, phone_number?, external_id?, id?, first_name?, ...}, properties?, value?, time?, unique_id?}` | `{id, profile_id}` |
| GET | `/api/events` | `?metric=&profile_id=&limit=&offset=` | `{data: EventRow[], total}` |
| GET | `/api/metrics` | – | `{data: {id, name, count_30d, count_prev_30d, last_seen, has_value, trend: number[30]}[]}` (`trend`: daily counts for the last 30 days, oldest first) |
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
| POST | `/api/forms` | `{name, kind, list_id, config, status?}` | `Form` (draft unless `status: "live"`) |
| GET/PATCH/DELETE | `/api/forms/:id` | partial | `Form`. PATCH merges `config` one level deep: sent keys replace, others are kept, `null` resets a key |
| POST | `/api/images` | the raw file with its image `Content-Type`, or JSON `{url}` or `{data_base64}` | `Image`. PNG, JPEG, GIF or WebP (checked from the bytes, not the claimed type; no SVG), up to 5 MB, 500 per account (409 past that). The same file again returns the existing image |
| GET | `/api/images` | – | `{data: Image[], total}` (newest 200) |
| DELETE | `/api/images/:id` | – | `{ok}` |

Form `config` (content, fields, phone step, style, teaser, targeting and schedule, A/B versions) is documented key by key in [FORMS.md](FORMS.md). Unknown keys and out-of-range values are rejected with a 400 that names the key and what it accepts.

## Analytics

| Method | Path | Query | Returns |
| --- | --- | --- | --- |
| GET | `/api/analytics/overview` | `?days=30` | `{days, totals: {revenue, attributed_revenue, campaign_revenue, flow_revenue, orders, attributed_orders, emails_sent, sms_sent, emails_delivered, opens, clicks, email_clicks, open_rate, click_rate, unsubscribe_rate, bounce_rate, complaint_rate, profiles, new_profiles}, previous: (the same totals for the `days` before, without `profiles`), series: {date, revenue, attributed_revenue, campaign_revenue, flow_revenue, emails_sent, sms_sent, opens, clicks, new_profiles}[], top_campaigns: {id, name, sent_at, recipients, open_rate, click_rate, orders, revenue}[], top_flows: {id, name, status, recipients, open_rate, click_rate, orders, revenue}[]}`. Flow numbers cover the range only. |
| GET | `/api/analytics/flows` | `?days=30` | `{days, totals: {revenue, orders, store_revenue, messages_sent, recipients, delivered, opens, clicks, open_rate, click_rate, revenue_per_recipient}, previous: (the same totals for the `days` before), series: {date, revenue, messages_sent}[]}`. Flow messages and flow-attributed orders only. |
| GET | `/api/analytics/forms` | `?days=30` | `{days, totals: {views, submits, submit_rate}, previous: (the same for the `days` before), series: {date, views, submits}[], forms: {id, name, views, submits, submit_rate}[]}`. Counts start when hourly form stats began (migration 0020); `forms[].views` and `submits` on `/api/forms` stay all-time. |

Attribution: by default a conversion event (`Placed Order`) is credited to the newest email the person opened or clicked in the previous 5 days, or the newest SMS they clicked in the previous 24 hours. Each account can change the windows, count clicks only, use first touch or count more metrics (`/api/reports/attribution`, see `docs/REPORTS.md`); every revenue number follows those settings.

## Public endpoints (no auth)

- `POST /client/events?k=<public_key>` – browser tracking (`{metric, profile, properties}`), CORS open.
- `POST /client/subscribe?k=<public_key>` – form submit, JSON or form-encoded: `{form_id, variant?, email?, phone_number?, first_name?, last_name?, sms_consent?, timezone?, fields?: {birthday?, <custom key>…}, step?: "phone", phone_token?}`. `timezone` is the browser's IANA zone and fills the profile's `timezone` and `country` properties when they're missing (the form renderer sends it). Answers are checked against the form's `fields` (see [FORMS.md](FORMS.md)): required fields, choices and types are enforced with a 400 naming the field, and answers the form doesn't ask for are ignored. Birthday and custom answers become profile properties; like names, they only fill blanks. Returns `{ok, message}`, plus `discount_code` when the form (or the visitor's A/B version) has one (public form configs leave the code out, so it's only handed over after a sign-up), `phone_token` when the form has a phone step and this sign-up created the profile, and `confirm: true` when the form's list is double opt-in and a confirmation email went out (the message then asks them to check their inbox; set `config.confirm_message` on the form to change it). `step: "phone"` (only when `config.phone_step.enabled`) needs `phone_number` and `sms_consent: true`, adds them to the person who signed up on the first screen when `phone_token` from that response is sent (good for an hour, and only if they have no number yet and nobody else has this one; otherwise the number is saved on its own profile, since an email address alone proves nothing), and doesn't count as a sign-up (no submit count, list join or event).
- `GET|POST /c/:token` – double opt-in confirmation. GET shows a "Yes, subscribe me" button; only the POST confirms, so link scanners can't. Links work for 7 days.
- `GET /f/:form_id` hosted sign-up page (drawn by the shared form renderer, with a plain HTML copy for browsers without JavaScript). Outside the form's `targeting.start_at`/`end_at` it says the form isn't open. `?preview=<token>` (from the form's `preview_url`) also shows drafts and ignores the schedule, counts no views and saves nothing; add `&v=b` to preview an A/B version. `GET /f/:form_id/embed.js` embed script.
- `GET /client/form-render.js` – the shared form renderer as `window.DumbMailFormRenderer` (CORS open, cached 5 minutes).
- `GET /i/:image_id` – an uploaded image, cached for a year (`immutable`), with `X-Content-Type-Options: nosniff` and `Content-Security-Policy: default-src 'none'`.
- `GET /t/o/:token` open pixel, `GET /t/c/:token?u=<url>` click redirect, `GET|POST /u/:token` unsubscribe (RFC 8058 one-click). For email, when the account has lists with `show_on_preferences`, GET shows a preference page instead: a checkbox per list, "Save preferences" (`action=save` with a `list` field per checked list; leaving a list records "Unsubscribed from List") and "Unsubscribe from all emails". Any POST without `action=save`, including one-click, unsubscribes from all email.
- `POST /webhooks/ses/:secret`, `POST /webhooks/telnyx/:secret`, `POST /webhooks/twilio/:secret` – delivery, bounce, complaint and inbound STOP.
- `POST /mcp` – Model Context Protocol (Streamable HTTP), `Authorization: Bearer pl_sk_...`.
- `GET /healthz` – `{ok: true, git_sha}` (the deployed commit; empty for local builds).

## Automatic email pause

Every account on a server sends through the same email provider account (Amazon SES on the hosted
service), which AWS reviews at a 5% bounce rate or 0.1% complaint rate. To protect everyone, an
account's email sending pauses automatically when either check below trips. Rates count real (non-test)
emails the provider accepted; bounces are permanent (hard) bounces only, not counting addresses already on
the provider's account suppression list. Sends and their outcomes are recorded separately from messages,
so deleting profiles (which deletes their messages) doesn't change the rates.

| Check | Window | Judged after | Pauses when |
| --- | --- | --- | --- |
| Account | emails created in the last 7 days, and since the pause was last lifted | 200 sent | hard bounces > 4% of sent, or at least 3 complaints and complaints > 0.08% of sent |
| Campaign (circuit breaker) | that campaign's emails, since the pause was last lifted | 100 sent | hard bounces > 10% of sent, or at least 3 complaints and complaints > 0.5% of sent |

Rates are recomputed when SES reports a bounce or complaint (at most every 10 seconds per account; the
scheduler catches up within about 20 seconds). While paused:

- `account.sending_paused_at` and `sending_paused_reason` are set (`/api/me`, MCP `get_account` as
  `email_sending_paused`), and the web app shows a banner.
- Email campaigns that are sending stop with `status: "failed"` and an `error` explaining the pause; their
  unsent messages become `skipped` with `error: "sending paused"`. Scheduled campaigns stop when they come
  due. New email campaigns can't be sent or scheduled (400, "Not ready to send: Email sending is paused…").
- Flow emails are recorded as `skipped` messages with `error: "sending paused"`; the flow carries on.
- Test emails return 409 with the reason.
- SMS is not affected.
- The operator is emailed at OPERATOR_ALERT_EMAIL (falling back to SUPPORT_EMAIL; the address sends to
  itself, so its domain must be verified with the email provider), and the pause is logged at error level.

## Sending guardrails

These protect the shared SES account's reputation along with the automatic pause above.

- **Warm-up limit (hosted service).** Real email (not tests) counts against a rolling 24-hour limit per
  account. New accounts start at `WARMUP_START_DAILY` (500). Each day the limit becomes twice the account's
  busiest finished day of the last 30 (in the account's time zone), as long as its last 7 days stay under
  2% hard bounces and (with 2 or more complaints) 0.03% complaints. Otherwise it holds at the busiest day.
  Today's sends never raise it. An account that sent nothing for 30 days starts over. Emails past the limit
  aren't dropped: campaign and flow emails wait (`queued`, the batch job is rescheduled) until the oldest
  send in the window turns 24 hours old, at least 15 minutes later. Operators can set a fixed limit with
  `POST /admin/sending-limit`.
- **Provider limits.** Sends never go faster than the SES account's own maximum rate (read at startup and
  every 10 minutes) or `SEND_RATE_PER_SEC`, whichever is lower. When SES throttles, the email is retried
  up to 3 times (1, 2, 4 seconds). When SES's daily quota is used up or sending is paused on SES's side,
  campaign and flow emails wait an hour instead of failing, and test sends return 409.
- **Unengaged profiles.** With `email_sunset_enabled` (on by default), email campaigns and their estimates
  leave out anyone who got 10 or more emails in the last 180 days and had no other event in that time:
  no open, click, sign-up, order or site activity. Flows aren't affected.
- **Imports** need `permission_confirmed` and skip role and throwaway addresses (see `/api/profiles/import`).
- **Double opt-in.** A form sign-up to a double opt-in list doesn't join the list. Someone new gets
  `email_consent: "pending"`, which no campaign or flow emails, until they confirm.
  A confirmation email (from the account's sender, not billed, no unsubscribe header) links to `/c/:token`.
  Confirming sets `email_consent: "subscribed"`, records "Subscribed to List" (with
  `double_opt_in: true`) and adds them to the list, which starts list-triggered flows. People already
  subscribed to email join straight away. An address waiting to confirm gets no second email for 24 hours, one inbox gets at most
  3 a day however it's spelled (`+tags` and Gmail dots ignored), one visitor (IP address) triggers at most 5 an hour, and an account sends at
  most 200 an hour (past that, sign-ups get 409 "try again later"). Addresses that bounced or complained before get
  none. If the account can't send email (no verified sender, or sending paused), the sign-up is refused
  with 409 ("try again later") rather than let through unconfirmed. Confirmation emails count toward bounce
  and complaint rates (a bounce suppresses the address) but not toward the warm-up limit.

## Operator endpoints

Require `Authorization: Bearer <ADMIN_TOKEN>` (32+ characters). When ADMIN_TOKEN is unset or shorter they
answer 404; a wrong token gets 401. Given `email`, the account is the one whose first member has that email.

| Method | Path | Body | Returns |
| --- | --- | --- | --- |
| POST | `/admin/credit` | `{email, amount, note?}` (USD; negative debits, at most 10,000) | `{account, balance}` |
| POST | `/admin/unpause` | `{account_id}` or `{email}` | `{account, was_paused, sending_paused: false}`. Lifts the automatic email pause and restarts the window the rates are measured over, so bounces from before don't pause the account again. Campaigns stopped by the pause stay failed |
| POST | `/admin/sending-limit` | `{account_id or email, daily_limit: number \| null}` | `{account, daily_limit, fixed, sent_last_24h}`. A fixed 24-hour email limit (0 to 10,000,000) in place of the warm-up, or `null` to return to it |

## v0.2 additions: pay-as-you-go billing, phone numbers, domains, website snippet

### Billing (hosted service; self-hosted installs return `mode: "self_hosted"` and never charge)

Money is prepaid credit. Each email or SMS segment sent is charged from the balance at cost plus markup.
Every account gets `free_emails_per_month` free emails each calendar month. SMS has no free allowance,
but texts sent through the account's own Telnyx or Twilio connection (`sms.mode: "byo"`) are never charged.
Monthly number rent is charged only when the balance covers it; otherwise the number gets a `release_at`
time (7 days out) and is released then unless the balance is topped up first.

| Method | Path | Body | Returns |
| --- | --- | --- | --- |
| GET | `/api/billing` | – | `Billing` (replaces the v0.1 shape) |
| POST | `/api/billing/checkout` | `{amount: number}` (USD, 25–5000) | `{url}`: redirect the browser to Stripe Checkout; it returns to `/settings/billing?topup=success` or `?topup=cancelled` |
| PATCH | `/api/billing/auto-recharge` | `{enabled: boolean, threshold: number, amount: number}` (amount 25–1000) | `Billing` |
| POST | `/api/billing/plan` | `{tier: 1-6}` | `{url: string \| null, billing: Billing}`: `url` is a Stripe Checkout page when something is due now (a new plan, or an upgrade's price difference); a downgrade or re-choosing the current tier is saved and `url` is null. Returns to `/settings/billing?plan=success` or `?plan=cancelled` |
| POST | `/api/billing/plan/cancel` | – | `Billing` (stops renewal; the plan lasts to `period_end`) |
| POST | `/api/billing/plan/resume` | – | `Billing` (undoes a cancellation before `period_end`) |

#### Monthly email plans

Optional, hosted only, dashboard only (API keys get 403). Tier N costs $10 × 2^(N-1) a month for
30,000 × 2^(N-1) emails, about 17% below pay-as-you-go. Emails use the free allowance first, then the
plan, then prepaid credit. An upgrade mid-month charges the price difference and adds the extra emails
at once; a downgrade starts at the next renewal. Renewals charge the saved card (no Stripe Billing),
one month later on the Pacific calendar. Each charge attempt is recorded (`plan_renewals`) with its
exact request before it's sent; an unknown outcome (network, 5xx, 409, 429) is settled by asking
Stripe before anything is resent, and Stripe's `payment_intent.succeeded` webhook settles it too.
A declined renewal is retried a day later, up to 3 tries, then the plan ends; while the renewal is
pending or failing, plan emails aren't available. Cancelling while a renewal is failing ends the plan
right away. Unused plan emails don't carry over. A plan payment that no longer fits by
the time it lands (e.g. an upgrade paid after the plan ended) is added to prepaid credit.

```ts
type Billing = {
  mode: "hosted" | "self_hosted";
  balance: number;                        // USD
  free_emails_per_month: number; emails_this_month: number; free_emails_remaining: number;
  sms_this_month: number;
  prices: { email_per_1000: number; sms_per_segment: number; mms_per_message: number; phone_number_monthly: number }; // USD
  auto_recharge: { enabled: boolean; threshold: number; amount: number };
  has_payment_method: boolean;            // true after a Checkout top-up saved a card
  stripe_enabled: boolean;                // false when the server has no Stripe key (show "contact us")
  usage: { date: string; kind: "email" | "sms" | "mms" | "number"; quantity: number; amount: number }[]; // last 30 days, daily
  ledger: { id: string; kind: string; description: string; amount: number; created_at: string }[];      // newest 50; credits positive
  limits: { profiles: number | null };
  plan: { status: "none"; ended_at: string | null; ended_reason: "cancelled" | "renewal_failed" | null }
      | { status: "active" | "cancelling" | "renewing" | "renewal_failed"; tier: number; price: number; emails_included: number; emails_used: number;
          emails_remaining: number; period_start: string; period_end: string; next_tier: number | null; next_price: number | null;
          next_emails: number | null; renewal_attempts: number; renewal_failed_at: string | null; max_renewal_attempts: number };
  plan_options: { tier: number; price: number; emails: number; price_per_1000: number; pay_as_you_go_price: number }[];
  plan_payments: { id: string; kind: "start" | "upgrade" | "renewal"; tier: number; amount: number; description: string; created_at: string }[]; // card payments, newest 24
  min_topup: number;                      // USD
};
```

Sending with an empty balance fails with error code `limit_reached` (HTTP 402) and a message saying how much credit is needed.

### Phone numbers and SMS

There are two ways to send texts:
- **platform**: claim a number through DumbMail. Toll-free numbers can send once carriers approve a toll-free verification, which is free and usually takes 1–5 business days. Texts are billed per segment.
- **byo**: connect your own Telnyx or Twilio account and number. DumbMail adds no fees; you pay your provider directly.

| Method | Path | Body / query | Returns |
| --- | --- | --- | --- |
| GET | `/api/phone-numbers` | – | `{data: PhoneNumber[], sms: SmsSetup}` |
| GET | `/api/phone-numbers/available` | `?type=toll_free\|local&area_code=503` | `{data: {phone_number, type, region: string \| null, monthly_price}[]}` |
| POST | `/api/phone-numbers` | `{phone_number}` | `PhoneNumber` (claims it, charges the first month, sets it as the account's SMS number) |
| DELETE | `/api/phone-numbers/:id` | – | `{ok: true}` (releases the number) |
| POST | `/api/phone-numbers/:id/verification` | `TollFreeVerification` | `PhoneNumber` |
| POST | `/api/sms/connect` | `{provider: "telnyx", api_key, phone_number, messaging_profile_id?}` or `{provider: "twilio", account_sid, auth_token, phone_number}` | `{data, sms}` (credentials are checked with the provider before saving, and stored encrypted) |
| DELETE | `/api/sms/connect` | – | `{data, sms}` (back to platform mode) |

```ts
type PhoneNumber = {
  id: string; phone_number: string; type: "toll_free" | "local"; source: "purchased" | "connected";
  provider: "telnyx" | "twilio";
  status: "active" | "needs_verification" | "verification_pending" | "verified" | "rejected" | "released";
  verification_note: string | null;  // carrier feedback when rejected
  monthly_price: number;             // 0 for connected numbers
  created_at: string;
  release_at: string | null;         // set when this month's rent is unpaid: released at this time unless credit is added
};
type SmsSetup = {
  mode: "platform" | "byo";
  provider: string;                  // provider that will send: telnyx | twilio | log
  from_number: string | null;        // the account's active sending number
  can_send: boolean;                 // number exists and is verified (or connected)
  inbound_webhook_url: string;       // BYO: paste into the provider's messaging webhook so STOP replies are recorded
  platform_available: boolean;       // server has a platform Telnyx key (else only BYO)
};
type TollFreeVerification = {
  business_name: string; website: string; address1: string; address2?: string; city: string; state: string; zip: string;
  contact_first_name: string; contact_last_name: string; contact_email: string; contact_phone: string;
  use_case: "Marketing" | "Mixed" | "Account Notifications" | "Customer Care" | "Delivery Notifications";
  use_case_summary: string; sample_message: string;
  opt_in_description: string;   // how people opt in, e.g. "Checkbox on our website sign-up form"
  opt_in_image_url: string;     // screenshot/URL of the opt-in, e.g. the hosted form page
  monthly_volume: "10" | "100" | "1,000" | "10,000" | "100,000";
};
```

In hosted mode `PATCH /api/account {sms_from}` only accepts one of the account's own numbers.

### Domains

An account can have up to 20 sending domains and may send from any address on a verified one (or a subdomain of it).
`GET /api/account/sending` lists them under `email.domains`; `email.domain`, `status`, `mail_from_domain` and `dns_records`
describe the main one (the default sender's domain, else the first verified), for older clients.

```ts
type SendingDomain = {
  domain: string; status: "pending" | "verified" | "failed"; mail_from_domain: string | null;
  dns_records: DnsRecord[]; created_at: string; verified_at: string | null;
  default_sender: boolean;  // the account's default from_email is on this domain
};
type Sending = {
  email: {
    provider: string; domain: string | null; from_email: string | null;
    from_email_verified: boolean;  // the default from_email is on a verified domain
    status: "not_started" | "pending" | "verified" | "failed";
    mail_from_domain: string | null;
    dns_records: DnsRecord[];
    domains: SendingDomain[];
    max_domains: number;
  };
  sms: { provider: string; from_number: string | null; status: string };
  link_domain: { domain: string | null; status: "not_started" | "pending" | "active" | "failed"; dns_records: DnsRecord[]; url: string | null };
};
type DnsRecord = {
  type: "CNAME" | "TXT" | "MX"; name: string; value: string; priority?: number;
  purpose: "dkim" | "spf" | "mail_from" | "dmarc" | "link" | "verification";
  status: "verified" | "pending" | "missing" | "recommended";
  found?: string | null;  // what public DNS returned, to help debug typos
};
```

| Method | Path | Body | Returns |
| --- | --- | --- | --- |
| POST | `/api/account/domains` | `{domain}` | `Sending` (adds the domain: DKIM, custom MAIL FROM `send.<domain>` for SPF/DMARC alignment, ownership TXT; re-checks it if already added) |
| POST | `/api/account/domains/{domain}/verify` | – | `Sending` (re-checks public DNS per record and with the provider) |
| DELETE | `/api/account/domains/{domain}` | – | `Sending` |
| POST | `/api/account/domain` | `{domain}` | Same as `POST /api/account/domains` (older name) |
| POST | `/api/account/domain/verify` | – | `Sending` (re-checks every domain) |
| DELETE | `/api/account/domain` | – | `Sending`; removes the only domain, 400 when there are several |
| POST | `/api/account/link-domain` | `{domain}` (e.g. `links.brand.com`) | `Sending` (branded tracking links, unsubscribe pages and hosted forms) |
| POST | `/api/account/link-domain/verify` | – | `Sending` |
| DELETE | `/api/account/link-domain` | – | `Sending` |

### Website snippet

One script tag goes in the site's `<head>` or footer:
`<script async src="{origin}/onsite.js?k={public_key}"></script>`

The script:
- tracks "Active on Site" once per visit;
- tracks shopping events by itself: "Viewed Product", "Added to Cart", "Started Checkout" and "Placed Order". It reads Shopify and WooCommerce stores directly, product structured data (JSON-LD, microdata, Open Graph) on any site, and Google Analytics ecommerce events (GA4 `view_item`, `add_to_cart`, `begin_checkout`, `purchase`, and Universal Analytics) in `dataLayer`. Single-page apps are followed through history changes;
- recognizes people who arrive from a tracked email or SMS link, sign up through a form, or give their own email on the site: the checkout email box (as they type) and sign-up, login, account, contact and similar forms with one email field (on submit). It skips fields for someone else's address (gift recipients, referrals, invites), forms with several email fields, and staff pages (`/admin`, `/wp-admin`, `/dashboard`…). Mark a field or form `data-dumbmail-identify` to always use it, or `data-dumbmail-ignore` to never use it. Recognition never changes consent;
- holds events from visitors it doesn't know yet in the browser for up to 3 days and sends them, with their original time, once the visitor is recognized;
- shows live popup and flyout forms using each form's targeting rules;
- renders embedded forms in any `<div data-dumbmail-form="FORM_ID"></div>`, including ones added to the page later;
- exposes `dumbmail.identify({email})`, `dumbmail.track(metric, properties, value)` and shopping helpers for custom sites: `viewedProduct(item)`, `addedToCart(item, cart?)`, `startedCheckout(cart)`, `placedOrder({order_id, total, items, email?})`. An item is `{name, product_id?, variant_id?, sku?, price?, quantity?, image_url?, url?, brand?, categories?}`; a cart is `{items, total?, currency?, checkout_url?}`. Calls queued before the script loads are kept.

Turn parts off with `window.dumbmailConfig = {autoTrack: false, captureEmails: false}` before the snippet, or `data-auto-track="false"` / `data-capture-emails="false"` on the script tag.

Shopify checkout pages can't run theme scripts, so Shopify stores also add a custom pixel (Settings → Customer events → Add custom pixel). `GET /api/website` returns its code as `shopify_pixel`. On Shopify the pixel is the only source of "Started Checkout" (sent once the buyer's email or phone is known, with a `checkout_url` back to the checkout) and "Placed Order"; both are credited to the email on the checkout, and activity the storefront held for that visitor is sent with them.

Shopping event properties: `Viewed Product` has the item fields; `Added to Cart` has the added item plus `items`, `item_names`, `cart_total`, `item_count` and `checkout_url`; `Started Checkout` has `items`, `item_names`, `total`, `item_count`, `currency` and `checkout_url`; `Placed Order` has `order_id`, `items`, `item_names`, `total`, `currency` and is de-duplicated by order id. Each also has `tracked_by` (where the data came from).

`POST /client/events` also accepts `time` (milliseconds since 1970) for events from the last 7 days.

**Test your setup.** `POST /api/website/tests` starts a 2-hour test session and returns `open_url` (the site with `?dumbmail_test=CODE`). The script first asks `GET /client/test?k=KEY&code=CODE` (`{active}`) and enters test mode only for a live test of its own account, so a made-up link does nothing. Opening it shows a test panel on the site and makes the script report what it detects to `POST /client/test` (16 KB limit; only known plain fields are kept) instead of saving events. Sign-up forms submitted in test mode don't subscribe anyone. `GET /api/website/tests/{code}` returns the results as `checks`, keyed by `loaded`, `forms`, `product`, `cart`, `checkout`, `order`, `identify`, `pixel`, `custom`, `wrong_key` (the site's snippet belongs to another account) and `warn_*` (duplicate snippet, blocked by Content-Security-Policy, rejected key, network blocked). Add `?dumbmail_test=off` to a page URL to leave test mode early.

| Method | Path | Body | Returns |
| --- | --- | --- | --- |
| GET | `/api/website` | – | `{snippet, shopify_pixel, public_key, installed: boolean, last_seen_at: string \| null, last_seen_url: string \| null}` |
| POST | `/api/website/check` | `{url}` | `{found: boolean, message: string, platform: string \| null}` (fetches the page server-side, looks for the snippet and guesses the site builder) |
| POST | `/api/website/tests` | `{url?}` (defaults to the account website) | `SiteTest` |
| GET | `/api/website/tests/{code}` | – | `SiteTest`: `{code, url, open_url, checks, created_at, expires_at, active, last_seen_at, last_url}` |

Forms have `config.targeting` (trigger, scroll depth, devices, page URLs, page views, new or returning visitors, frequency, hiding for known subscribers, and a `start_at`/`end_at` schedule); see the Targeting section of [FORMS.md](FORMS.md). `GET /client/forms` leaves out forms outside their schedule.
