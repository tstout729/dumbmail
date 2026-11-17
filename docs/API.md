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
| GET | `/api/auth/options` | – | `{google, signup}`: which sign-in methods this server offers |
| GET | `/api/auth/google?next=/path` | – | Redirects to Google ("Continue with Google"). The callback (`/api/auth/google/callback`) signs in, joins an existing user with the same email (turning its password off and ending older sessions), or creates a new account and sends it to `/onboarding`. Failures redirect to `/login?error=<code>` |
| GET | `/api/me` | – | `{user, account}` (401 if signed out) |
| PATCH | `/api/account` | any `Account` editable fields, plus `onboarding` partial. `social_links` is `{instagram\|facebook\|tiktok\|x\|youtube\|pinterest\|linkedin\|threads: https URL}`; a blank value removes that network. Social links appear in email footers | `Account` |
| POST | `/api/brand/import` | `{url, apply?}` | `{brand: {website, name, logo_url, brand_color, colors[], address, from_email, from_email_found, social_links}}`. Reads the public website (and its stylesheets and logo) for brand suggestions. With `apply: true`, also saves website, logo, brand color and social links, plus name and address when the account has none, and returns `account` |
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
| POST | `/api/billing/checkout` | `{amount: number}` (USD, 10–5000) | `{url}`: redirect the browser to Stripe Checkout; it returns to `/settings/billing?topup=success` or `?topup=cancelled` |
| PATCH | `/api/billing/auto-recharge` | `{enabled: boolean, threshold: number, amount: number}` | `Billing` |

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

Forms gain `config.targeting`; every field is optional:

```ts
type Targeting = {
  trigger: "delay" | "scroll" | "exit_intent";   // default "delay" (uses config.delay_seconds)
  scroll_percent?: number;                       // for "scroll", default 40
  devices?: "all" | "desktop" | "mobile";         // default "all"
  url_contains?: string[];                       // show only on pages whose URL contains one of these
  url_excludes?: string[];                       // never show on these
  frequency_days?: number;                       // after someone closes it, wait this long (default 7)
  hide_for_subscribers?: boolean;                // default true: skip people already identified on this browser
};
```
