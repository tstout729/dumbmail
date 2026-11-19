# Custom integrations

An integration connects any app to DumbMail. The app sends webhooks to the integration's secret URL, or DumbMail fetches from the app's API on a schedule (a *pull*). A small JavaScript function you write, the *script*, turns each payload into a list of *actions*: create or update profiles, track events, change lists, record consent. Every step is logged, failures retry, and anything can be replayed.

Ready-made integrations (*recipes*) exist for WooCommerce, BigCommerce, Stripe, Square, Recharge, Paddle, Guesty, Hostaway, Cloudbeds, Calendly, Acuity Scheduling, Eventbrite, Typeform, Jotform, Webflow forms, Zendesk, Gorgias, AfterShip, ShipStation and Segment (see Recipes). Start from one when it fits (`list_integration_recipes`, `create_integration_from_recipe`); otherwise build one with the steps below.

## Build one in five steps

1. `create_integration` with a name, a script and the `metrics` (event names) the script records. The answer includes `inbound_url`, the secret URL the app should POST to.
2. Find out how the app signs its webhooks and set `signature` and `signature_secret` (see Signatures).
3. `test_integration` with a real sample payload from the app's docs. It returns exactly what would be written, and writes nothing. Fix the script until it's right.
4. Point the app at `inbound_url` (in its dashboard or through its API).
5. Watch `list_integration_deliveries`. Fix the script and `replay_integration_deliveries` with `all_failed: true` if anything failed.

For apps without webhooks, or to import history, add a `pull` (see Pulls) and test it with `test_integration` and `pull: true`.

## Who can do what

- **Managers, admins and the owner** (`integrations.read`) see integrations, their status and the delivery log: each delivery's time, status, error and counts. API keys need the `account` scope (`account:read` to read); plain `read` isn't enough, because an integration's URL is itself a credential.
- **Admins and the owner** (`integrations.manage`) create, change and delete integrations, their secrets and credentials, test and run them, replay deliveries, import history, install recipes, and see the inbound URL, saved state, stored payloads and what the script wrote (personal data).
- Creating, changing and deleting integrations, changing their secrets, replays and history imports are recorded in the audit log (field names and credential names, never values or payloads).
- Only the inbound URL (`POST /webhooks/in/{token}`) is public. It's checked by its token and the integration's signature settings.

## Consent: people an integration adds are held back from marketing

An integration usually learns about people because they booked or bought, not because they asked for marketing. So a person an integration **creates** without an explicit opt-in is put on a *marketing hold*:

- Campaigns never email them, and segments' `can_receive` email rule is false for them.
- Flows email them only when the flow is marked as service messages (`definition.settings.service_messages: true`): messages about their own booking or order, like a booking confirmation or arrival details. Never use it for promotions.
- Unsubscribed and suppressed people get nothing, service messages or not.
- The hold clears the moment they subscribe by any route: a sign-up form, double opt-in, the API, or a `consent` block from the integration.
- It's never set on someone who already existed, and the profile page says "Hasn't agreed to marketing. Added by <integration>." It's set in the same step that creates the profile, so a failure never leaves an unheld one behind.
- A service-message flow reaches a held person only at the email address an integration supplied. If the address changes any other way, they get nothing more until they opt in.
- A back-in-stock alert the person asked for and confirmed still reaches them (it doesn't clear the hold).
- A web form or the website script never adds or changes an email or phone on a profile an integration wrote when it matched that profile by another identifier: a different email gets its own profile. Consent from a form applies only to the address or number typed into it.

If everyone the app sends really did agree to marketing email from this business (for example a newsletter tool), set `contacts_opted_in: true` on the integration. Only do this when the user confirms it.

Events an integration records are trusted by flows. So each integration declares the metrics its script records (`metrics`, up to 50), and they become *server-only* the moment the integration is saved: the public browser endpoint (`/client/events`) refuses them for that account, even before the first real one arrives. The script can record only declared metrics (anything else fails the delivery with a message saying so), and never DumbMail's own alerts (`Back in Stock`, `Price Dropped`, `Subscribed to Back in Stock`). The private API and MCP can still record them. When a trusted event (from DumbMail, the API or an integration) has the same `unique_id` as one a browser sent earlier, it replaces the browser's copy, so flows treat it as new.

## The script

The script defines `function handle(input)` and returns an array of actions. It runs in a sandbox with no network, files, timers or modules: it only turns data into actions. Plain ES2023 JavaScript; `async` is not allowed.

```js
function handle(input) {
  var order = input.payload;
  if (order.status !== "paid") return [{ type: "log", message: "not paid yet" }];
  return [
    { type: "upsert_profile", email: order.email, first_name: order.first_name },
    { type: "track_event", metric: "Placed Order", profile: { email: order.email },
      value: order.total, unique_id: "shop:" + order.id, properties: { order_id: order.id } },
  ];
}
```

`input` has:

| Field | What it is |
| --- | --- |
| `source` | `webhook`, `pull` or `test` |
| `replay` | `true` when a stored delivery is being run again |
| `payload` | The body: parsed JSON, a form as an object (a `multipart/form-data` body is read as a form of its text fields; file parts are dropped), otherwise the text |
| `body` | The raw body text |
| `headers` | Request headers, lower-case names. `authorization` and `cookie` are never kept. |
| `query` | Query string parameters |
| `settings` | The integration's `settings` (non-secret configuration you choose) |
| `state` | Values saved with `set_state` (cursors, counters) |
| `now` | The current time, ISO 8601 UTC |
| `integration` | `{id, name}` |
| `response`, `request`, `page`, `backfill` | Pulls only: the app's answer status and headers, the request made (secrets hidden), the page number (from 1) and `{since}` during a backfill, otherwise `null` |
| `response`, `request`, `context` | Fetches (`source` `fetch`): the app's answer status, headers and `received_at`, the request made (secrets hidden) with `sent_at` (when DumbMail sent it: a later request for the same record is only sent once an earlier answer is stored, so a script can version what an answer says by it), and the `context` the webhook's `next_request` carried |

Return `[]` (or nothing) when there's nothing to do; the delivery is logged as `ignored`.

## Actions

Every action is checked before anything is written. If one is invalid, the delivery fails with a message naming it (for example `action 2 (track_event): unknown field "colour"`) and nothing is written. Unknown fields are rejected.

Where an action takes a `profile`, give identifiers (`email`, `phone_number`, `external_id`; the profile is created if missing) or a lookup by a custom property, `{"property": "stripe_customer_id", "value": "cus_123"}`, which never creates and skips the action when nobody matches. Add `"if_missing": "retry"` to fail the delivery instead, so it runs again later (after 1, 5, 30 and 120 minutes), for example when a subscription event arrives before the order that tells us who the customer is.

### upsert_profile

```json
{"type": "upsert_profile", "email": "ann@example.com", "phone_number": "+15035550100", "external_id": "u_42",
 "first_name": "Ann", "last_name": "Lee", "properties": {"plan": "gold"},
 "consent": [{"channel": "email", "subscribed": true, "source": "Checkout marketing checkbox"}],
 "marketing": false}
```

- Needs at least one of `email`, `phone_number` (E.164, like `+15035550100`), `external_id`. Matching order: external id, then email, then phone.
- `properties` merge into the profile's properties.
- A `consent` block may carry `as_of`, the app's own time for the preference. The newest time the integration sent for that channel and address is kept whether or not the preference changed anything, and an older one is ignored (an older "yes" can't follow a newer "no"). With `subscribed: false`, `only_own: true` withdraws a subscription only when this integration made the current one: for the same address, still subscribed, and with no unsubscribe, list sign-up or other source's opt-in since. Consent from anywhere else is left alone. The check and the change are one statement.
- **Consent never changes unless you send a `consent` block**, which must name the `channel` (`email` or `sms`), `subscribed` (`true` or `false`) and the `source` of the opt-in. Use it only when the person actually opted in through the app. Subscribing never overrides an earlier unsubscribe. A subscription records a "Subscribed to Email Marketing" (or SMS) event with the source.
- `marketing: false` suppresses the email address from all marketing email, with a reason shown on the profile. Use it for addresses that must never get marketing.
- Booking-site relay addresses (Airbnb `@guest.airbnb.com`, Booking.com `@guest.booking.com`, Vrbo/HomeAway, Expedia and others) are detected automatically on every profile an integration writes and suppressed from email, with the reason "booking-site relay address". Their terms forbid marketing to them.
- Profiles an integration creates are marked with it as their source (shown on the profile), and held back from marketing unless the action opts them in or the integration has `contacts_opted_in` (see Consent).
- `keys` (`{"stripe_payment_intent": "pi_123"}`): ids that identify the person in the app and can be many (payments, bookings). Each value is added to the person's list for that key (the newest 100 are kept), not shown as a property, and found later with `profile: {"property": "stripe_payment_intent", "value": "pi_123"}`. Plain property values the integration writes can be looked up the same way. A value belongs to one profile: one another profile already has isn't added (the log says so).
- `match_key` names one of `keys`: when nobody has the `external_id` yet, the one profile already holding that key's value is the one updated (people keyed before the integration used an external id).
- `keep_email: true`: an existing profile that has an email keeps it, so an app that can't prove an address change (anyone with its write key can send a call) doesn't move someone's mail elsewhere. The log notes a call that named another address, and an email `consent` block in that call is skipped (it was given for the other address).
- A match by phone or external id that brings a different email changes the address. The new address hasn't agreed to anything, so a subscribed profile becomes `never_subscribed`. Unsubscribed stays unsubscribed, a hard bounce (it belonged to the old address) is cleared, and a spam complaint stays.

### track_event

```json
{"type": "track_event", "metric": "Reservation Confirmed", "profile": {"email": "ann@example.com"},
 "properties": {"reservation_id": "r_1", "check_in": "2026-12-17"}, "value": 1312.5,
 "time": "2026-11-15T19:04:11Z", "unique_id": "guesty:r_1:confirmed"}
```

- `metric` up to 128 characters. Events start metric-triggered flows and power segments.
- `metric` must be one of the integration's `metrics`.
- `value` is revenue (a number). `time` is ISO 8601 (default: when the script first ran for this delivery).
- Always set `unique_id`: the same metric and unique id is recorded once, so a resent webhook or an overlapping pull never counts twice. Without one, the event gets an id from its delivery and position, so one delivery (with its retries and replays) still never records it twice.
- For something that changes over time (a booking's dates), record a new event per version, with `time` set to the app's own update time and the version in the `unique_id`. Flows keyed on the thing follow the newest by time, so a late, older version never replaces a newer one. Count revenue on a separate event that has one `unique_id` per order or booking, so a change never counts it again.

### add_to_list / remove_from_list

```json
{"type": "add_to_list", "list": "Guests", "profile": {"email": "ann@example.com"}}
{"type": "remove_from_list", "list_id": "0193...", "profile": {"email": "ann@example.com"}}
```

`list` is the list's exact name, or give `list_id`. The list must exist; a missing list fails the delivery before anything is written. Adding to a list starts that list's flows. List membership is not consent.

### unsubscribe

```json
{"type": "unsubscribe", "channel": "email", "profile": {"email": "ann@example.com"}}
```

Records an unsubscribe on `email` or `sms`, as if they clicked the link.

### suppress

```json
{"type": "suppress", "email": "ann@example.com"}
{"type": "suppress", "phone_number": "+15035550100"}
```

The address must never get marketing: an email is suppressed, a phone number is unsubscribed from SMS. Creates the profile if needed.

### add_keys

```json
{"type": "add_keys", "profile": {"property": "square_customer", "value": "C1", "if_missing": "retry"}, "keys": {"square_payment": "P1"}}
```

Adds ids to the `keys` of a person found another way, so later actions find them by these ids too (a payment that names only its customer, so the refund that names only the payment finds them). Up to 20 keys, as in `upsert_profile`. A value belongs to one profile: if another profile in the account already has it, that key isn't added (the delivery's log says so) and the rest are.

### lookup

```json
{"type": "lookup", "url": "/wp-json/wc/v3/orders/727/refunds", "context": {"order": {"id": 727}}}
```

Like a webhook's `next_request`, but there can be up to 100, in a pull page as well as a webhook (a history import looking up each order's refunds, say). Each is checked against the integration's `fetch` rules and reserved the same way; the script runs on each answer with `source` `fetch`.

### upsert_object / delete_object

```json
{"type": "upsert_object", "object_type": "reservation", "external_id": "R-1042",
 "fields": {"status": "confirmed", "check_in": "2026-12-01", "check_out": "2026-12-05", "nights": 4},
 "profiles": [{"email": "ana@example.com", "relationship": "guest"}],
 "as_of": "2026-11-17T10:00:00Z"}
{"type": "delete_object", "object_type": "reservation", "external_id": "R-1042"}
```

Creates or updates a custom object (see dumbmail://docs/objects): fields merge, `null` removes one, every change is recorded and can start object flows. The type is made on first use with field types guessed from the values. `profiles` find people like `profile` in other actions (email, phone_number, external_id, or property + value with `if_missing`); someone new is held back from marketing as above, and a lookup that finds nobody leaves the object unlinked. `as_of` is the app's own time or version for this state: an older one never overwrites a newer one. `only_if_as_of_at_most` (instead of `as_of`) writes evidence about an existing object as of a moment, only if it hasn't changed since (the Guesty recipe's payment check uses it). The integration's `redact` list is applied to `fields` before they're written, so `**.doorCode` keeps a door code out of objects too. An object without an `external_id` gets one from the delivery (stable across retries). Counted as `objects_upserted` and `objects_deleted`; tests and dry runs say what would change.

### set_state

```json
{"type": "set_state", "values": {"cursor": "2026-11-17T10:00:00Z", "old_key": null}}
```

Saves values for the next run (`input.state`, and `{{state.KEY}}` in pull settings). `null` removes a key. State is capped at 64 KB.

### log

```json
{"type": "log", "message": "skipped owner stay r_9"}
```

A note in the delivery log.

### next_request (pulls, and webhooks that fetch)

```json
{"type": "next_request", "query": {"skip": 200}}
{"type": "next_request", "url": "/v1/items?page=3"}
{"type": "next_request", "url": "/stores/abc123/v2/orders/250", "context": {"refund_id": 12}}
```

In a pull, fetch another page. In a webhook, look up the record it names: many apps (BigCommerce, Acuity, Eventbrite) send only an id. A webhook's lookup is read-only and declared up front in the pull settings, which also give the site, headers and credentials:

```json
"fetch": {"prefix": "/stores/{{settings.store_hash}}/", "paths": ["/stores/{{settings.store_hash}}/v2/orders/{id}"], "query": ["id:in"], "id": "^[0-9]{1,20}$", "context": ["refund"]}
```

- Always a `GET`. The script gives only `url` (a path), `query` and `context`; `method`, `headers` or `body` are refused.
- The path must match one of `paths` exactly, where `{id}` is one path segment matching `id` (digits by default) and `{{settings.KEY}}` is filled in; query keys must be listed in `query`, with values that match `id`, or listed as `key=value` to allow exactly that value (`expand=refund_requests`). A path ends with `/` only if the declared one does (Eventbrite's do).
- Every lookup stays under `prefix` (default `/`; the recipes name the part of the vendor's API they use). Each path in `paths` must start with it, and a lookup whose path, filled in and normalised, isn't under it is refused, even on the same site.
- Only the `context` keys listed are kept; the context is put through the redact list's removals, sealed (it isn't shown in the delivery's request record), counted in the payload budget, and handed to the run on the answer as `input.context` (up to 16 KB).
- The lookup is reserved in the same step that marks the webhook done: a delivery row (source `fetch`) that counts against the limits on work waiting to run, unique on the record (its path and query) while it's outstanding, so a second webhook for the same record joins it, and its context replaces the first one's. Only the job holding it makes the request, never before the row's due time (a retry wait or the app's cooldown), and only once the payload budget has room for a whole answer: that room is reserved before the request and held until the answer is stored, so a request that worked is never repeated for lack of space.
- An answer that says "not yet" (404, 409, 425, 429, a server error) or a lost connection is tried again after 1, 5, 30 and 120 minutes, or after the wait the app asked for (BigCommerce's `X-Rate-Limit-Time-Reset-Ms`, then `Retry-After`, then the header named in `reset_header`, ShipStation's `X-Rate-Limit-Reset`; up to two hours). That wait holds for every lookup of the integration (its credentials), not only the one that was refused.
- Budget and rate: an integration queues at most `per_hour` lookups an hour (default 1,000, at most 10,000), counted in the database as each one is reserved; past it, the webhook or page that asked waits and is tried again. `per_minute` (the app's documented rate, ShipStation's 40) keeps its requests to that many a minute; a lookup over it waits its turn.
- `identity`: by default a lookup is the record (its path and query), and a second callback's context replaces the first's while it waits. With `record_and_context` each context is its own piece of work (ShipStation's lookups of an order carry a batch's shipments, and a second batch must never replace them).
- The answer is kept on that row (redacted) and the script runs on it with `input.request`, `input.response` and `input.context`. That run may look up once more (up to 3 lookups from one webhook), for example a refund, then its order. A replay runs the current script again, which decides afresh what to look up.

## Signatures

`signature` says how to check that a webhook came from the app. The secret goes in `signature_secret`; it's stored encrypted and never returned (only a hint like `…3f9a`). Requests that fail the check get HTTP 401 and are logged without their body.

| Setting | Use for |
| --- | --- |
| `{"type": "none"}` | Apps that can't sign. The long random URL is the only secret. |
| `{"type": "hmac", "header": "X-Hub-Signature-256", "prefix": "sha256="}` | HMAC-SHA256 of the raw body, hex, after a prefix (GitHub style) |
| `{"type": "hmac", "header": "X-Shopify-Hmac-Sha256", "encoding": "base64"}` | HMAC-SHA256, base64 |
| `{"type": "hmac", "algorithm": "sha1", "header": "X-Signature"}` | HMAC-SHA1 |
| `{"type": "hmac", "header": "X-Signature", "timestamp_header": "X-Timestamp", "tolerance_seconds": 300}` | HMAC of `timestamp.body`, rejecting old timestamps |
| `{"type": "hmac", "header": "Stripe-Signature", "format": "stripe"}` | `t=…,v1=…` headers (Stripe, Calendly): HMAC-SHA256 of `t.body`, 5-minute tolerance |
| `{"type": "hmac", "header": "svix-signature", "format": "svix"}` | Svix / Standard Webhooks (Guesty and many others): `v1,<base64>` over `id.timestamp.body` with a `whsec_` secret. Use `webhook-signature` for Standard Webhooks headers. |
| `{"type": "hmac", "header": "Paddle-Signature", "format": "paddle"}` | `ts=…;h1=…` (Paddle): HMAC-SHA256 of `ts:body`, 5-minute tolerance |
| `{"type": "hmac", "header": "x-square-hmacsha256-signature", "encoding": "base64", "sign_url": true}` | HMAC of the webhook URL (exactly as the app was given it) followed by the body (Square) |
| `{"type": "hmac", "header": "x-webflow-signature", "timestamp_header": "x-webflow-timestamp", "timestamp_separator": ":", "timestamp_unit": "milliseconds"}` | HMAC of `timestamp:body` with a millisecond timestamp (Webflow). `timestamp_separator: ""` and `timestamp_unit: "iso8601"` cover Zendesk's `timestamp + body` with an ISO 8601 time. |
| `{"type": "shared_secret", "header": "X-Webhook-Token"}` | The secret itself in a header |
| `{"type": "shared_secret", "query": "token"}` | The secret in a query parameter (removed before storing) |

Other options for `hmac`: `secret_encoding: "base64"` when the secret is base64 (plain format), `tolerance_seconds` 10 to 172800 (48 hours, as Recharge documents). All comparisons are constant-time.

The signature is checked before anything else, so requests without the secret never use up the integration's allowance. Rejected requests are logged only while they're few (30 a minute per integration); the rest are refused without a log line.

**Resends.** Most apps retry a webhook with the same message id. Set `message_id` to where it is, `{"header": "svix-id"}` or `{"field": "id"}` (dots reach nested keys), and a second delivery with the same id is answered 200 with `"duplicate": true` and not run again. Add `"within_minutes": 10` when the id is a hash of the content that a later, real change can repeat (BigCommerce's callback `hash`): a copy within that window is a resend, a later one is new.

## Pulls

A pull fetches from the app's API and runs the script on each page. It's declared as data:

```json
{"method": "GET", "url": "https://api.example.com/v1/orders",
 "query": {"updated_since": "{{state.cursor|since}}", "limit": 100},
 "headers": {"X-Region": "{{settings.region}}"},
 "auth": {"type": "bearer", "token": "{{secret.api_key}}"},
 "interval_minutes": 15, "max_pages": 10}
```

- `interval_minutes` (5 or more) runs it on a schedule. Leave it out to pull only on request or for a backfill.
- `max_pages` caps pages per scheduled run (default 10, up to 50). `backfill_max_pages` caps a backfill run (default 5,000).
- `body` (JSON, or a form with `body_format: "form"`) for POST requests.

**The site is fixed.** The URL's scheme, host and port are plain text, written when you save it: no placeholders there, no `user:password@`, and `https` only. Placeholders go in the path, query, headers and body.

**Placeholders** in the URL's path and query, headers, body and auth:

| Placeholder | Value |
| --- | --- |
| `{{secret.NAME}}` | A credential saved with `credentials` (write-only, encrypted) |
| `{{state.KEY}}` | A value the script saved with `set_state` |
| `{{settings.KEY}}` | A value from `settings` |
| `{{now}}`, `{{now_unix}}` | The current time (ISO 8601 UTC, or Unix seconds) |
| `{{since}}`, `{{since_unix}}` | In a backfill, its start date; otherwise 24 hours ago |
| `{{a\|b\|'text'}}` | The first one that has a value: `{{state.cursor\|since}}` |

**Auth.** Credentials live only in `credentials` (encrypted, write-only) and are referenced, never written into the settings: every auth field must be exactly a reference like `{{secret.api_key}}` (saving a literal value is refused), and a custom header whose name looks like a credential (`…key`, `…token`, `…secret`, `…auth…`) must use one too. The settings are returned with the references, never the values. These headers can't be set at all, here or by `next_request`: `Host`, `Authorization` (it comes from `auth`), `Proxy-*`, `Forwarded`, `X-Forwarded-*`, `Connection`, `Transfer-Encoding`, `Content-Length`, `Cookie`.

- `{"type": "bearer", "token": "{{secret.api_key}}"}`
- `{"type": "basic", "username": "{{secret.user}}", "password": "{{secret.password}}"}`
- `{"type": "header", "name": "X-Api-Key", "value": "{{secret.api_key}}"}`
- `{"type": "oauth2_client_credentials", "token_url": "https://…/oauth2/token", "client_id": "{{secret.client_id}}", "client_secret": "{{secret.client_secret}}", "scope": "…", "audience": "…", "client_auth": "body" | "basic", "max_tokens_per_day": 4}`. The token URL, scope and audience are plain text (no placeholders).

OAuth2 tokens are cached (encrypted) and reused until five minutes before they expire. The cache is tied to everything that decides the token: the token URL, client id, client secret, scope, audience and the pull's site. Change any of them and the cached token is dropped; a token is only ever sent to the site it was configured for. Re-saving the same values keeps it. A new token is requested only when no matching one is cached, only one run asks at a time, and never more than `max_tokens_per_day` in 24 hours (default 4; Guesty allows 5). A run that started before the settings changed stops instead of caching its token. If the app answers 401 or 403, the token is dropped and the next run gets a new one.

**Safety:** saved state, settings and secrets are percent-encoded in a URL and can't change its site. Requests go only to public internet addresses (no private, loopback, link-local or cloud metadata addresses), the address is pinned after it's checked, no proxy is used, redirects aren't followed, each request times out after 20 seconds, and answers over 5 MB are refused. Errors that are stored or shown give the site and path (query values hidden), the HTTP status and a short reason, never the app's own error text, and any saved secret is masked wherever it would appear.

**Paging:** the script decides. Return `next_request` to fetch the next page. Move the cursor forward on every page (for example "changed at or after the last row's update time"), so a run that stops at its page limit picks up where it left off.

**Backfill:** `run_integration_pull` with `backfill_since: "2025-01-01"` (and `confirm: true`) imports history: the same pull with `{{since}}` set to that date and its own empty state, run in pages as chained jobs until the script stops asking for more. Progress (pages, state and the next request) is saved after every page, and only by the run that owns it. `get_integration` shows `backfill` (`running`, `done`, `capped`, `failed`, `cancelled`, pages). A run that reaches `backfill_max_pages` stops as `capped`, not `done`; `run_integration_pull` with `resume_backfill: true` (also after `failed`) continues from where it got to. `cancel_integration_backfill` stops it. A backfill never moves the live cursor.

## Deliveries, retries and replays

Every webhook, pull page, fetch and replay is a *delivery* with its time, source (`webhook`, `pull`, `fetch`, `replay`), status (`pending`, `ok`, `error`, `ignored`), error, what was written (counts and a line per action), duration and payload.

- A webhook is checked, stored and queued in one step, and answered with 200 in milliseconds; the script runs right after in the background.
- A failed webhook delivery retries after 1, 5, 30 and 120 minutes (five tries), then stays failed. Failed pull pages aren't retried: the next scheduled pull fetches again. A page fetched again
  before its progress was saved (after a crash or a failure) carries on with the earlier attempt's
  record of what it already wrote, so those actions aren't applied twice: a delete it already made
  never removes an object created since under the same id.
- The first time a delivery's script runs successfully, its list of actions is kept with the delivery (missing `unique_id`s and `time`s filled in). A retry applies exactly that list; it never runs the script again, so a script whose output depends on the time can't record something twice. Each delivery also records which actions it already wrote, and a retry or replay skips those (`already_written` in the counts).
- Nothing secret is stored or shown: every credential value, the access token, the encoded Basic value and anything else a request sent are masked wherever they appear (payloads, response headers such as `Link`, errors, the script's actions and logs, test results), including short ones (a PIN is masked where it stands as a whole word). The next-page request between pages is kept encrypted.
- `replay_integration_deliveries` runs stored deliveries again with the current script: pick `delivery_ids`, or `all_failed: true` for every delivery that failed for good and hasn't been replayed (up to 500 at a time). Needs `confirm: true`. Replays count against the same limits as new webhooks.
- While an integration is turned off, a webhook gets a log line (as `ignored`), but nothing is written and its payload isn't kept.
- **Payloads may contain personal data** (names, addresses, booking notes). Only admins and the owner can read them (`integrations.manage`). Set `redact` to remove fields before a payload is stored or shown: dot paths like `results[].notes` or `reservation.notes`, `[]` for each item of a list, `*` for every key and `**` for any depth (`**.keyCode`). Start the list with `keep:` paths to make it an allow-list: only the leaves they name are stored (a path that ends on an object or a list of objects keeps it empty; name the leaves under it), then the other paths are removed from that. Allow-list mode never turns off: if none of its paths resolves (a setting it uses is empty), nothing is kept. A test run gives the script the payload as it would be stored, so its actions and logs show no more than a real run. A key may filter array items, `answers[field/ref=a|b]`, and `{setting:KEY}` stands for a setting's value (or each item of a list), so `keep:payload.data.{setting:email_field}` follows what the installer mapped. A pull's script sees the whole page; a webhook's (or fetch's) script runs on what was stored, so redact only fields the script doesn't read (the recipes' test suite checks this for every fixture). A payload that isn't JSON or a form can't be checked, so with a redact list it isn't kept. Recipes come with their list (Guesty's removes door codes and access notes).
- Payloads are kept at most 30 days, and each account keeps at most 256 MB of them. A new payload makes room by dropping the oldest finished ones (their log lines stay); when payloads still waiting to run fill the budget, the webhook is answered 429 so the app sends it again later (a pull page keeps its log line without the page).
- Scripts run in their own short-lived process with no network, files, open descriptors or environment. At most 2 run at once per account (8 in total by default); work over that waits a few seconds in the queue.
- All integration work (deliveries, retries, pulls, backfills) runs in the background queue, apart from sending. A scheduled pull isn't queued while one for the same integration is waiting or running, and one pull runs at a time per account (4 in all by default, `INTEGRATION_PULL_SLOTS`).

## Flows from integration events

Events from integrations start flows like any other events, and `{{ event.properties.* }}` works in the emails.

- **Metric trigger with cancellation:** `{"type": "metric", "metric": "Reservation Confirmed", "key": "reservation_id", "cancelled_by": ["Reservation Cancelled"]}`. People leave the flow before the next step if a "Reservation Cancelled" with the same `reservation_id` comes later, or a newer "Reservation Confirmed" with the same id replaces this one.
- **Event date trigger:** `{"type": "event_date", "metric": "Reservation Confirmed", "field": "check_in", "offset_days": -2, "hour": 10, "key": "reservation_id", "cancelled_by": ["Reservation Cancelled"], "filters": []}` starts the flow 2 days before each event's `check_in`, at 10am in the account's time zone. Each event starts it once, so a guest with two bookings gets both. A plain date (`2026-12-17`) is used as is; a date-time is read in the account's time zone. The same cancellation and replacement rules apply, checked when it fires and again before each step. `offset_days` is -365 to 365; `hour` 0-23 (default 9); at most 20 filters.
- Each event is scheduled when it's recorded (and existing events when the flow goes live or its trigger changes, up to 20,000 events a minute across all flows). Due reminders go out before older events are scheduled. Anything due in the last 24 hours still goes out; a reminder that was due longer ago, whether the scheduler was down or the event arrived (or was imported) late, is marked `missed` instead of sending a stale one. A newer version of the same booking (same `key`) that didn't move its date doesn't send the reminder again (`already_sent`). When the account's time zone changes, waiting reminders move to the same hour in the new one. `get_flow_report` shows the counts under `event_dates` (`pending`, `started`, `superseded`, `cancelled`, `already_sent`, `filtered`, `missed`).
- Mark flows that are about the person's own booking or order `"service_messages": true` in `definition.settings`, so they reach guests held back from marketing (see Consent).
- Mark old records you import (for example `historical: true` in the event's properties) and filter them out of "right after it happened" flows with `{"field": "historical", "op": "neq", "value": true}`, so a backfill doesn't email everyone about old bookings.

## Limits

| Limit | Value |
| --- | --- |
| Webhook body | 1 MB (larger: 413) |
| Webhooks per integration | 300 a minute (more: 429 with `Retry-After`), and 5,000 waiting to run at once, retries and replays included |
| Webhooks per account | 1,200 a minute, and 20,000 waiting to run at once |
| Script size | 256 KB |
| Script run | 2 seconds, 64 MB of memory, 512 KB of stack; its process is killed when exceeded |
| Scripts at once | 2 per account, 8 in total (`INTEGRATION_SCRIPT_SLOTS`) |
| Script result | 1 MB, 500 actions |
| Properties per action | 200 keys, 64 KB |
| Saved state | 64 KB |
| Declared metrics, redact paths | 50 each |
| Pull answer | 5 MB, 20-second timeout |
| Pull interval | 5 minutes or more |
| Integrations per account | 50 |
| Delivery log | 30 days; 256 MB of payloads per account, checked when each one arrives |
| Pulls at once | 1 per account, 4 in total (`INTEGRATION_PULL_SLOTS`); fetches share these slots |
| Fetches from one webhook | 3, each `context` up to 16 KB |
| Lookups per integration | 1,000 an hour by default (`per_hour`, up to 10,000) |

## Recipes

A recipe is a ready-made integration kept in the DumbMail repository under `recipes/<key>/`: `recipe.json` (name, description, signature, pull, settings, the metrics it records, the credentials it needs, setup steps and suggested flows; a metric is a name, or `{"name": ..., "purchase": true}` for one that records a purchase or a stay, which makes a review asked about it verified), `script.js`, and `fixtures/` with sample payloads and the actions they must produce. `create_integration_from_recipe` copies it into the account and creates the suggested flows as drafts (nothing sends until someone turns them on). Add one by pull request; the test suite runs every fixture.

- **guesty**: v2 reservation webhooks and the current Open API (`/v1/reservations-v3/search` and `/v1/guests-crud`) → Reservation Booked (once per booking), Reservation Total (the booking's value, once), Reservation Confirmed (one per version of its dates, listing, status or guest), Reservation Cancelled, Reservation Updated and Inquiry Received, ordered by Guesty's own update time; door codes and access notes removed from stored payloads; guests as profiles held back from marketing (relay addresses suppressed); history import with client-credentials (token reuse); each reservation kept as a `reservation` object linked to its guest (status, dates, listing, nights, guests, channel; written with Guesty's time so an older message never wins); and draft booking confirmation, pre-arrival, review request and object-based pre-arrival flows marked as service messages. `recipes/guesty/README.md` lists the Guesty doc page behind each assumption, and what still has an end date.
- **stripe**: Checkout (including delayed payments), invoices, refunds and subscriptions → Placed Order (not for checkouts that needed no payment), Refunded Order (once per refund, its own amount, once it has succeeded, found through its PaymentIntent or charge), Subscription Started (when it becomes active, once) / Cancelled; buyers who tick Checkout's promotional-emails box are subscribed, everyone else is held back from marketing.
- **calendly**: bookings and cancellations → Meeting Scheduled / Meeting Cancelled, with draft reminder and follow-up flows.

Every recipe below was checked against the vendor's documentation on 2026-11-17 (Cloudbeds, Acuity Scheduling, Eventbrite, Jotform, Gorgias and ShipStation on 2026-11-18) and hasn't yet run against a live account. Each recipe's `README.md` lists the doc page behind each assumption, what's removed before payloads are stored, and what a live check should look at first. None of them subscribes anyone without an opt-in field the vendor documents for that purpose; everyone else they add is held back from marketing.

### Commerce and subscriptions

- **woocommerce**: order and customer webhooks (`X-WC-Webhook-Signature`, base64 HMAC) → Placed Order once it's paid, Fulfilled Order, Cancelled Order (paid orders only), Refunded Order (once per refund id, its own amount). Pending, on-hold, failed and free orders aren't orders. Past orders import with a REST API key once the Pull tab has the store's address. Setup: four webhooks (Order created, Order updated, Customer created, Customer updated) under WooCommerce → Settings → Advanced → Webhooks, with one secret.
- **bigcommerce**: BigCommerce webhooks carry only an id, so each one fetches the order, refund or customer from the store's API (see `next_request`). Placed Order once paid, Fulfilled Order, Cancelled Order, Refunded Order (once per refund, in the order's currency, unless every payment in it was declined). The checkout's email box (`is_email_opt_in`) subscribes the buyer only when the installer sets `checkout_opt_in_is_marketing`, confirming it's an unticked marketing opt-in. Setup: a store-level API account (read-only Orders, Order transactions, Customers), `store_hash` in the settings, and one webhook per scope created with the API, carrying a secret `X-Webhook-Token` header.
- **recharge**: Subscription Started, Reactivated, Cancelled and Skipped, Upcoming Charge, Charge Paid, Charge Failed. Recharge's body doesn't say which topic sent it, so each webhook is created with `?topic=<topic>` on the URL, and the topic must fit the signed body. Subscriptions wait for their customer. Draft upcoming-charge reminder (service message).
- **square**: Placed Order for completed payments, Refunded Order for completed refunds (found through the payment), customers as profiles; Square's "unsubscribed from marketing emails" suppresses the address. Square signs the webhook URL too, so paste it exactly.
- **paddle** (Paddle Billing): Placed Order for completed transactions, Refunded Order for approved refunds, Subscription Started and Cancelled; customers who tick Paddle Checkout's marketing box (`marketing_consent`) are subscribed. Draft win-back (marketing). Picked over Chargebee, whose webhooks use basic auth instead of a signature.

### Hospitality and bookings

- **hostaway**: reservations read from Hostaway's API every 15 minutes (its webhooks' body isn't documented): Reservation Booked, Reservation Total, Reservation Confirmed (each version, with `paid_in_full` as Hostaway reports it), Reservation Cancelled, Inquiry Received. Door codes, lockbox instructions and wifi details are removed from stored pages and never reach events. Draft pre-arrival flow that fires only for bookings paid in full, with placeholders, not access details. Setup: the Hostaway account ID and API key as credentials.
- **cloudbeds**: Cloudbeds' reservation webhooks only name the reservation (and are checked by a secret header, since Cloudbeds doesn't sign them; their timestamp isn't signed either), so each one fetches it from Cloudbeds' API: Cloudbeds Reservation Confirmed (once per reservation), Cloudbeds Reservation Cancelled and Cloudbeds Checked Out, on the main guest, with no money and no purchase marker. Each reservation is kept as a `cloudbeds_reservation` object (its own type, so flows built for other booking systems never act on it), versioned by when DumbMail sent the request, every field written each time; `paid_in_full` only on exact decimal evidence (a booked status, a total above zero, a grand total equal to it, a balance of exactly zero and payments equal to the total). A deletion beats every answer requested before it was handled. An hourly check fetches every confirmed arrival in the next week again, since a payment sends no reservation webhook. No starter flows. Guest documents, addresses, cards and custom fields are never stored. Setup: an API key, and four webhooks created with Cloudbeds' API.
- **acuity**: Acuity Scheduling's signed, form-encoded webhooks name an appointment; it's fetched, and what Acuity says now is recorded: Appointment Scheduled (once), Appointment Rescheduled (once per new time) and Appointment Cancelled (once, with `no_show`), so a late webhook can't bring back a cancelled appointment. Acuity's appointments have no revision number, so each webhook records the latest state: quick back-and-forth moves can show fewer reschedules. Intake forms, notes and prices are never stored. Setup: the user ID and API key (also the signing secret), and the webhook URL in Integrations → Webhooks.
- **eventbrite**: Eventbrite's webhooks aren't signed and only point at an order or attendee (the integration's URL is the secret); each one is fetched with your private token, only for the events listed in `event_ids`, and only pointers into Eventbrite's API are followed. Money only from Eventbrite's records, in minor units at the currency's real precision: Placed Order for a placed order with a gross and a payment processor fee, Registered for Event (no value) for any other placed order, and Refunded Order once per processed refund item of the order's refund request, for its processed amount. Checked In once per attendee. Barcodes and custom answers are never stored.

### Forms, support, shipping and data

- **typeform** and **webflow**: Form Submitted on the respondent's profile. A form submission never subscribes anyone and never renames an existing profile (anyone can type an address into a form); a consent question or checkbox you map in the settings is recorded on the event as `marketing_consent`. Only the fields you map (email, consent, `copy_fields`) and the form and submission ids are stored; everything else is dropped first. Webflow signs only webhooks created through its API, so the setup creates it that way.
- **jotform**: the same for Jotform, whose webhooks aren't signed: each one only gives the submission id (its multipart body's answers are dropped), and the submission is fetched with your API key, only for the form in `form_id` (required). Questions are mapped by their IDs (`email_field`, `consent_field` with `consent_value`, `copy_fields`); only names, email, phone, short text, numbers, dates, dropdowns, radio buttons and checkboxes can be mapped, and only their values are kept.
- **zendesk**: Ticket Created, Ticket Solved and Satisfaction Rated from triggers that post a JSON body the setup gives (Zendesk placeholders). Only the subject and status are kept, never the conversation, and requesters' names aren't taken. Event ids come from the signed body (ticket, status or rating, update time), so identical solves within one minute count once.
- **gorgias**: Ticket Created, Ticket Closed (each closing) and Satisfaction Rated (with the score), from an HTTP integration whose body only names the ticket and carries a secret header; the ticket is fetched from Gorgias's API. Gorgias's HTTP integrations have no trigger for survey answers, so a rating arrives only with the ticket's next update. Only the subject, status and channel are kept, never messages or the survey comment, and customers' names aren't taken. Setup: the API key, and the Pull tab's address set to your Gorgias account.
- **aftership**: Shipment In Transit, Out For Delivery, Delivered and Shipment Exception, once per shipment and status, for the shipment's customer (not the sender). Draft out-for-delivery email (service message).
- **shipstation**: ShipStation's webhooks aren't signed and only point at a batch of shipments (the integration's URL is the secret); the batch is fetched with your API key and secret, a page at a time, then each shipped order for its customer's email, only for the stores in `store_ids`. Order Shipped once per shipment (not voided or return labels); each order's lookup carries its own shipments, so overlapping batches never lose one. It keeps to ShipStation's 40 requests a minute and waits as long as ShipStation says. A batch of more than 50 pages fails visibly. Draft shipping email (service message; one per person at a time).
- **segment**: identify calls (with `traits.email`) become profiles and keep the Segment userId as a key (a known userId's email isn't changed by a call); track calls become events for the names in `settings.events` (Segment's ecommerce events by default), found by that userId. Calls from Segment's browser and mobile libraries are refused unless `allow_client_sources` is on, because a browser source's write key is public.


## Worked example: "connect my Guesty"

1. `create_integration_from_recipe` with `{"recipe": "guesty"}`. Note `integration.inbound_url` and the `setup` steps.
2. Ask the person for their Guesty Open API client ID and secret (Guesty → Integrations → API & Webhooks), then `update_integration` with `{"id": …, "credentials": {"client_id": "…", "client_secret": "…"}}`.
3. Subscribe the inbound URL in Guesty. With their Open API token: `POST https://open-api.guesty.com/v1/webhooks` with `{"url": "<inbound_url>", "events": ["reservation.created.v2", "reservation.updated.v2"]}`. These update guests DumbMail already knows at once (cancellations too); the 15-minute pull adds new guests.
4. Get the signing secret (`GET https://open-api.guesty.com/v1/webhooks-v2/secret`, starts with `whsec_`) and `update_integration` with `{"signature_secret": "whsec_…"}`.
5. `test_integration` with `{"pull": true}`. The first page is changed reservations, and the answer asks for their guests next (`next_request` to `/v1/guests-crud`). If Guesty answers 400, the reservation filter needs adjusting (see the recipe's README).
6. With the person's go-ahead, `run_integration_pull` with `{"backfill_since": "<a year ago>", "confirm": true}`. Watch `get_integration` until `backfill.status` is `done`.
7. Show them the three draft flows (`list_flows`), adjust the emails to their voice, `render_preview`, and turn them on with `set_flow_status` once they approve. They're service messages, so they reach guests who haven't agreed to marketing; keep them about the stay.

## Writing a script for a new app

- Read the app's webhook docs for the exact payload and signature scheme; copy a real sample into `test_integration`.
- Build `unique_id` from the app's own ids (`"app:" + order.id + ":paid"`), so retries and pulls never double count.
- Check fields exist before using them, and skip or `log` what you don't handle instead of throwing.
- Clean phone numbers to E.164 before sending them (an invalid one fails the whole delivery).
- Never subscribe anyone without an explicit opt-in in the payload, and say where it came from in `source`.
- Set `message_id` when the app gives each webhook an id, so its retries are ignored.
