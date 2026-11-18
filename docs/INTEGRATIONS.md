# Custom integrations

An integration connects any app to DumbMail. The app sends webhooks to the integration's secret URL, or DumbMail fetches from the app's API on a schedule (a *pull*). A small JavaScript function you write, the *script*, turns each payload into a list of *actions*: create or update profiles, track events, change lists, record consent. Every step is logged, failures retry, and anything can be replayed.

Ready-made integrations (*recipes*) exist for Guesty, Stripe and Calendly. Start from one when it fits (`list_integration_recipes`, `create_integration_from_recipe`); otherwise build one with the steps below.

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
| `payload` | The body: parsed JSON, a form as an object, otherwise the text |
| `body` | The raw body text |
| `headers` | Request headers, lower-case names. `authorization` and `cookie` are never kept. |
| `query` | Query string parameters |
| `settings` | The integration's `settings` (non-secret configuration you choose) |
| `state` | Values saved with `set_state` (cursors, counters) |
| `now` | The current time, ISO 8601 UTC |
| `integration` | `{id, name}` |
| `response`, `request`, `page`, `backfill` | Pulls only: the app's answer status and headers, the request made (secrets hidden), the page number (from 1) and `{since}` during a backfill, otherwise `null` |

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
- **Consent never changes unless you send a `consent` block**, which must name the `channel` (`email` or `sms`), `subscribed` (`true` or `false`) and the `source` of the opt-in. Use it only when the person actually opted in through the app. Subscribing never overrides an earlier unsubscribe. A subscription records a "Subscribed to Email Marketing" (or SMS) event with the source.
- `marketing: false` suppresses the email address from all marketing email, with a reason shown on the profile. Use it for addresses that must never get marketing.
- Booking-site relay addresses (Airbnb `@guest.airbnb.com`, Booking.com `@guest.booking.com`, Vrbo/HomeAway, Expedia and others) are detected automatically on every profile an integration writes and suppressed from email, with the reason "booking-site relay address". Their terms forbid marketing to them.
- Profiles an integration creates are marked with it as their source (shown on the profile), and held back from marketing unless the action opts them in or the integration has `contacts_opted_in` (see Consent).
- `keys` (`{"stripe_payment_intent": "pi_123"}`): ids that identify the person in the app and can be many (payments, bookings). Each value is added to the person's list for that key (the newest 100 are kept), not shown as a property, and found later with `profile: {"property": "stripe_payment_intent", "value": "pi_123"}`. Plain property values the integration writes can be looked up the same way.
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

### upsert_object / delete_object

```json
{"type": "upsert_object", "object_type": "reservation", "external_id": "R-1042",
 "fields": {"status": "confirmed", "check_in": "2026-12-01", "check_out": "2026-12-05", "nights": 4},
 "profiles": [{"email": "ana@example.com", "relationship": "guest"}],
 "as_of": "2026-11-17T10:00:00Z"}
{"type": "delete_object", "object_type": "reservation", "external_id": "R-1042"}
```

Creates or updates a custom object (see dumbmail://docs/objects): fields merge, `null` removes one, every change is recorded and can start object flows. The type is made on first use with field types guessed from the values. `profiles` find people like `profile` in other actions (email, phone_number, external_id, or property + value with `if_missing`); someone new is held back from marketing as above, and a lookup that finds nobody leaves the object unlinked. `as_of` is the app's own time or version for this state: an older one never overwrites a newer one. The integration's `redact` list is applied to `fields` before they're written, so `**.doorCode` keeps a door code out of objects too. An object without an `external_id` gets one from the delivery (stable across retries). Counted as `objects_upserted` and `objects_deleted`; tests and dry runs say what would change.

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

### next_request (pulls only)

```json
{"type": "next_request", "query": {"skip": 200}}
{"type": "next_request", "url": "/v1/items?page=3"}
```

Fetch another page. `query` and `headers` merge into the previous request (`null` removes a query parameter); `url`, `method` and `body` replace it. A `url` may change only the path and query: the site (scheme, host, port) stays the pull's own, and it can't carry a user name or password. Values are used as they are (placeholders and secrets are not expanded), and the same headers as in the pull settings are refused, so a payload can't steer the request or its credentials elsewhere.

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
| `{"type": "shared_secret", "header": "X-Webhook-Token"}` | The secret itself in a header |
| `{"type": "shared_secret", "query": "token"}` | The secret in a query parameter (removed before storing) |

Other options for `hmac`: `secret_encoding: "base64"` when the secret is base64 (plain format), `tolerance_seconds` 10 to 86400. All comparisons are constant-time.

The signature is checked before anything else, so requests without the secret never use up the integration's allowance. Rejected requests are logged only while they're few (30 a minute per integration); the rest are refused without a log line.

**Resends.** Most apps retry a webhook with the same message id. Set `message_id` to where it is, `{"header": "svix-id"}` or `{"field": "id"}` (dots reach nested keys), and a second delivery with the same id is answered 200 with `"duplicate": true` and not run again.

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

Every webhook, pull page and replay is a *delivery* with its time, source (`webhook`, `pull`, `replay`), status (`pending`, `ok`, `error`, `ignored`), error, what was written (counts and a line per action), duration and payload.

- A webhook is checked, stored and queued in one step, and answered with 200 in milliseconds; the script runs right after in the background.
- A failed webhook delivery retries after 1, 5, 30 and 120 minutes (five tries), then stays failed. Failed pull pages aren't retried: the next scheduled pull fetches again.
- The first time a delivery's script runs successfully, its list of actions is kept with the delivery (missing `unique_id`s and `time`s filled in). A retry applies exactly that list; it never runs the script again, so a script whose output depends on the time can't record something twice. Each delivery also records which actions it already wrote, and a retry or replay skips those (`already_written` in the counts).
- Nothing secret is stored or shown: every credential value, the access token, the encoded Basic value and anything else a request sent are masked wherever they appear (payloads, response headers such as `Link`, errors, the script's actions and logs, test results), including short ones (a PIN is masked where it stands as a whole word). The next-page request between pages is kept encrypted.
- `replay_integration_deliveries` runs stored deliveries again with the current script: pick `delivery_ids`, or `all_failed: true` for every delivery that failed for good and hasn't been replayed (up to 500 at a time). Needs `confirm: true`. Replays count against the same limits as new webhooks.
- While an integration is turned off, a webhook gets a log line (as `ignored`), but nothing is written and its payload isn't kept.
- **Payloads may contain personal data** (names, addresses, booking notes). Only admins and the owner can read them (`integrations.manage`). Set `redact` to remove fields before a payload is stored or shown: dot paths like `results[].notes` or `reservation.notes`, `[]` for each item of a list, `*` for every key and `**` for any depth (`**.keyCode`). The script still sees the whole payload. A payload that isn't JSON or a form can't be checked, so with a redact list it isn't kept. Recipes come with their list (Guesty's removes door codes and access notes).
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
| Pulls at once | 1 per account, 4 in total (`INTEGRATION_PULL_SLOTS`) |

## Recipes

A recipe is a ready-made integration kept in the DumbMail repository under `recipes/<key>/`: `recipe.json` (name, description, signature, pull, settings, the credentials it needs, setup steps and suggested flows), `script.js`, and `fixtures/` with sample payloads and the actions they must produce. `create_integration_from_recipe` copies it into the account and creates the suggested flows as drafts (nothing sends until someone turns them on). Add one by pull request; the test suite runs every fixture.

- **guesty**: v2 reservation webhooks and the current Open API (`/v1/reservations-v3/search` and `/v1/guests-crud`) → Reservation Booked (once per booking), Reservation Total (the booking's value, once), Reservation Confirmed (one per version of its dates, listing, status or guest), Reservation Cancelled, Reservation Updated and Inquiry Received, ordered by Guesty's own update time; door codes and access notes removed from stored payloads; guests as profiles held back from marketing (relay addresses suppressed); history import with client-credentials (token reuse); each reservation kept as a `reservation` object linked to its guest (status, dates, listing, nights, guests, channel; written with Guesty's time so an older message never wins); and draft booking confirmation, pre-arrival, review request and object-based pre-arrival flows marked as service messages. `recipes/guesty/README.md` lists the Guesty doc page behind each assumption, and what still has an end date.
- **stripe**: Checkout (including delayed payments), invoices, refunds and subscriptions → Placed Order (not for checkouts that needed no payment), Refunded Order (once per refund, its own amount, once it has succeeded, found through its PaymentIntent or charge), Subscription Started (when it becomes active, once) / Cancelled; buyers who tick Checkout's promotional-emails box are subscribed, everyone else is held back from marketing.
- **calendly**: bookings and cancellations → Meeting Scheduled / Meeting Cancelled, with draft reminder and follow-up flows.

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
