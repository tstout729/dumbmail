# Custom integrations

An integration connects any app to DumbMail. The app sends webhooks to the integration's secret URL, or DumbMail fetches from the app's API on a schedule (a *pull*). A small JavaScript function you write, the *script*, turns each payload into a list of *actions*: create or update profiles, track events, change lists, record consent. Every step is logged, failures retry, and anything can be replayed.

Ready-made integrations (*recipes*) exist for Guesty, Stripe and Calendly. Start from one when it fits (`list_integration_recipes`, `create_integration_from_recipe`); otherwise build one with the steps below.

## Build one in five steps

1. `create_integration` with a name and a script. The answer includes `inbound_url`, the secret URL the app should POST to.
2. Find out how the app signs its webhooks and set `signature` and `signature_secret` (see Signatures).
3. `test_integration` with a real sample payload from the app's docs. It returns exactly what would be written, and writes nothing. Fix the script until it's right.
4. Point the app at `inbound_url` (in its dashboard or through its API).
5. Watch `list_integration_deliveries`. Fix the script and `replay_integration_deliveries` with `all_failed: true` if anything failed.

For apps without webhooks, or to import history, add a `pull` (see Pulls) and test it with `test_integration` and `pull: true`.

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

Where an action takes a `profile`, give identifiers (`email`, `phone_number`, `external_id`; the profile is created if missing) or a lookup by a custom property, `{"property": "stripe_customer_id", "value": "cus_123"}`, which never creates and skips the action when nobody matches.

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
- Profiles an integration creates are marked with it as their source (shown on the profile).

### track_event

```json
{"type": "track_event", "metric": "Reservation Confirmed", "profile": {"email": "ann@example.com"},
 "properties": {"reservation_id": "r_1", "check_in": "2026-12-17"}, "value": 1312.5,
 "time": "2026-11-15T19:04:11Z", "unique_id": "guesty:r_1:confirmed"}
```

- `metric` up to 128 characters. Events start metric-triggered flows and power segments.
- `value` is revenue (a number). `time` is ISO 8601 (default now).
- Always set `unique_id`: the same metric and unique id is recorded once, so a resent webhook or an overlapping pull never counts twice.

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

Fetch another page. `query` and `headers` merge into the previous request; `url`, `method` and `body` replace it. The URL must stay on the same site as the pull's URL, and the values are used as they are (placeholders and secrets are not expanded), so a payload can't steer credentials elsewhere.

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
- `max_pages` caps pages per scheduled run (default 10, up to 50).
- `body` (JSON, or a form with `body_format: "form"`) for POST requests.

**Placeholders** in the URL, query, headers, body and auth:

| Placeholder | Value |
| --- | --- |
| `{{secret.NAME}}` | A credential saved with `credentials` (write-only, encrypted) |
| `{{state.KEY}}` | A value the script saved with `set_state` |
| `{{settings.KEY}}` | A value from `settings` |
| `{{now}}`, `{{now_unix}}` | The current time (ISO 8601 UTC, or Unix seconds) |
| `{{since}}`, `{{since_unix}}` | In a backfill, its start date; otherwise 24 hours ago |
| `{{a\|b\|'text'}}` | The first one that has a value: `{{state.cursor\|since}}` |

**Auth:**

- `{"type": "bearer", "token": "{{secret.api_key}}"}`
- `{"type": "basic", "username": "{{secret.user}}", "password": "{{secret.password}}"}`
- `{"type": "header", "name": "X-Api-Key", "value": "{{secret.api_key}}"}`
- `{"type": "oauth2_client_credentials", "token_url": "https://…/oauth2/token", "client_id": "{{secret.client_id}}", "client_secret": "{{secret.client_secret}}", "scope": "…", "audience": "…", "client_auth": "body" | "basic", "max_tokens_per_day": 4}`

OAuth2 tokens are cached (encrypted) and reused until five minutes before they expire. A new token is requested only when no valid one is cached, and never more than `max_tokens_per_day` in 24 hours (default 4; Guesty allows 5). Changing the credentials or the pull's auth drops the cached token; re-saving the same values keeps it. If the app answers 401 or 403, the token is dropped and the next run gets a new one.

**Safety:** requests go only to public internet addresses (no private, loopback, link-local or cloud metadata addresses), the address is pinned after it's checked, redirects aren't followed, each request times out after 20 seconds, and answers over 5 MB are refused.

**Paging:** the script decides. Return `next_request` to fetch the next page. Hold a cursor steady while paging: save the newest time seen as `pending_cursor` on each page and commit it as `cursor` on the last page, so the query doesn't change mid-way.

**Backfill:** `run_integration_pull` with `backfill_since: "2025-01-01"` (and `confirm: true`) imports history: the same pull with `{{since}}` set to that date and its own empty state, run in pages as chained jobs until the script stops asking for more (up to 5,000 pages). `get_integration` shows `backfill` progress (`running`, `done`, `cancelled`, `failed`, pages). `cancel_integration_backfill` stops it. A backfill never moves the live cursor.

## Deliveries, retries and replays

Every webhook, pull page and replay is a *delivery* with its time, source (`webhook`, `pull`, `replay`), status (`pending`, `ok`, `error`, `ignored`), error, what was written (counts and a line per action), duration and payload. They're kept 30 days.

- A webhook is checked, stored and answered with 200 in milliseconds; the script runs right after in the background.
- A failed webhook delivery retries after 1, 5, 30 and 120 minutes (five tries), then stays failed. Failed pull pages aren't retried: the next scheduled pull fetches again.
- `replay_integration_deliveries` runs stored deliveries again with the current script: pick `delivery_ids`, or `all_failed: true` for every delivery that failed for good and hasn't been replayed (up to 500 at a time). Needs `confirm: true`.
- While an integration is turned off, webhooks are still stored (as `ignored`) so they can be replayed later.

## Flows from integration events

Events from integrations start flows like any other events, and `{{ event.properties.* }}` works in the emails.

- **Metric trigger with cancellation:** `{"type": "metric", "metric": "Reservation Confirmed", "key": "reservation_id", "cancelled_by": ["Reservation Cancelled"]}`. People leave the flow before the next step if a "Reservation Cancelled" with the same `reservation_id` comes later, or a newer "Reservation Confirmed" with the same id replaces this one.
- **Event date trigger:** `{"type": "event_date", "metric": "Reservation Confirmed", "field": "check_in", "offset_days": -2, "hour": 10, "key": "reservation_id", "cancelled_by": ["Reservation Cancelled"], "filters": []}` starts the flow 2 days before each event's `check_in`, at 10am in the account's time zone. Each event starts it once, so a guest with two bookings gets both. A plain date (`2026-12-17`) is used as is; a date-time is read in the account's time zone. The same cancellation and replacement rules apply, checked when it fires and again before each step. `offset_days` is -365 to 365; `hour` 0-23 (default 9).
- Mark old records you import (for example `historical: true` in the event's properties) and filter them out of "right after it happened" flows with `{"field": "historical", "op": "neq", "value": true}`, so a backfill doesn't email everyone about old bookings.

## Limits

| Limit | Value |
| --- | --- |
| Webhook body | 1 MB (larger: 413) |
| Webhooks per integration | 300 a minute (more: 429 with `Retry-After`), and 5,000 waiting at once |
| Script size | 256 KB |
| Script run | 2 seconds, 64 MB of memory, 512 KB of stack; stopped when exceeded |
| Script result | 1 MB, 500 actions |
| Properties per action | 200 keys, 64 KB |
| Saved state | 64 KB |
| Pull answer | 5 MB, 20-second timeout |
| Pull interval | 5 minutes or more |
| Integrations per account | 50 |
| Delivery log | 30 days |

## Recipes

A recipe is a ready-made integration kept in the DumbMail repository under `recipes/<key>/`: `recipe.json` (name, description, signature, pull, settings, the credentials it needs, setup steps and suggested flows), `script.js`, and `fixtures/` with sample payloads and the actions they must produce. `create_integration_from_recipe` copies it into the account and creates the suggested flows as drafts (nothing sends until someone turns them on). Add one by pull request; the test suite runs every fixture.

- **guesty**: reservation webhooks and the Open API → Reservation Confirmed / Cancelled / Updated and Inquiry Received, guests as profiles (relay addresses suppressed), history import with client-credentials (token reuse), and draft booking confirmation, pre-arrival and review request flows.
- **stripe**: Checkout, invoices, refunds and subscriptions → Placed Order, Refunded Order, Subscription Started / Cancelled; buyers who tick Checkout's promotional-emails box are subscribed.
- **calendly**: bookings and cancellations → Meeting Scheduled / Meeting Cancelled, with draft reminder and follow-up flows.

## Worked example: "connect my Guesty"

1. `create_integration_from_recipe` with `{"recipe": "guesty"}`. Note `integration.inbound_url` and the `setup` steps.
2. Ask the person for their Guesty Open API client ID and secret (Guesty → Integrations → API & Webhooks), then `update_integration` with `{"id": …, "credentials": {"client_id": "…", "client_secret": "…"}}`.
3. Subscribe the inbound URL in Guesty. With their Open API token: `POST https://open-api.guesty.com/v1/webhooks` with `{"url": "<inbound_url>", "events": ["reservation.new", "reservation.updated"]}`. (These carry the guest's details; Guesty retires them on 1 March 2027, after which the 15-minute pull fills in guest details for v2 events.)
4. Get the signing secret (`GET https://open-api.guesty.com/v1/webhooks-v2/secret`, starts with `whsec_`) and `update_integration` with `{"signature_secret": "whsec_…"}`.
5. `test_integration` with `{"pull": true}`. Check `effects`: one profile and one "Reservation Confirmed" per reservation, relay guests marked suppressed.
6. With the person's go-ahead, `run_integration_pull` with `{"backfill_since": "<a year ago>", "confirm": true}`. Watch `get_integration` until `backfill.status` is `done`.
7. Show them the three draft flows (`list_flows`), adjust the emails to their voice, `render_preview`, and turn them on with `set_flow_status` once they approve.

## Writing a script for a new app

- Read the app's webhook docs for the exact payload and signature scheme; copy a real sample into `test_integration`.
- Build `unique_id` from the app's own ids (`"app:" + order.id + ":paid"`), so retries and pulls never double count.
- Check fields exist before using them, and skip or `log` what you don't handle instead of throwing.
- Clean phone numbers to E.164 before sending them (an invalid one fails the whole delivery).
- Never subscribe anyone without an explicit opt-in in the payload, and say where it came from in `source`.
