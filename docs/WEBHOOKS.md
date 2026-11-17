# Webhooks

DumbMail can POST what happens in your account to your own systems: tracked events, profile and
consent changes, list and segment membership, message outcomes, form sign-ups, campaigns, flows and
imports. Set them up in **Settings → Webhooks**, through the API (`/api/webhooks`) or with the MCP
tools (`create_webhook` and friends). Zapier and Make use the same deliveries through REST hooks.

Payloads carry personal data (email addresses, phone numbers, properties). DumbMail only sends to
`https://` URLs on the public internet: never to private networks, localhost or cloud metadata
addresses, and redirects aren't followed. Each endpoint gets only its own account's data.

## Topics

| Topic | Sent when | `data` |
| --- | --- | --- |
| `event.tracked` | Any event is recorded (API, website, forms, messages). Set `metrics` to get only those | `{event: {id, metric, properties, value, occurred_at, unique_id, message_id}, profile}` |
| `profile.created` | A new profile is added | `{profile: Profile}` |
| `profile.updated` | A profile's details, properties or consent change (saving the same values sends nothing) | `{profile: Profile, changed: string[]}` |
| `profile.deleted` | A profile is deleted | `{profile: {id, email, phone_number, external_id}}` |
| `profile.subscribed` | Someone subscribes to email or SMS | `{channel: "email" \| "sms", reason, profile}` |
| `profile.unsubscribed` | Someone unsubscribes from email or SMS | `{channel, reason, message_id?, profile}` |
| `profile.suppressed` | An email address is suppressed after a hard bounce or spam complaint | `{channel: "email", reason: "hard_bounce" \| "complaint", message_id, profile}` |
| `list.member_added` | A profile joins a list | `{list: {id, name}, profile}` |
| `list.member_removed` | A profile leaves a list | `{list: {id, name}, profile}` |
| `segment.entered` | A profile enters a segment | `{segment: {id, name}, profile}` |
| `segment.left` | A profile leaves a segment | `{segment: {id, name}, profile}` |
| `message.sent` | An email or text is sent | `{message, profile}` |
| `message.delivered` | The provider confirms delivery | `{message, profile}` |
| `message.opened` | An email is opened (first open) | `{message, profile}` |
| `message.clicked` | A link in an email or text is clicked | `{message, url, profile}` |
| `message.bounced` | An email bounces | `{message, bounce_type: "Permanent" \| "Transient" \| ..., profile}` |
| `message.complained` | Someone marks an email as spam | `{message, profile}` |
| `form.submitted` | Someone signs up through a form | `{form: {id, name, kind}, list_id, variant, profile}` |
| `campaign.sent` | A campaign starts sending | `{campaign: {id, name, channel, status, sent_at, recipients}}` |
| `campaign.finished` | A campaign has finished sending | same as `campaign.sent` |
| `flow.status_changed` | A flow is turned on, paused or set to draft | `{flow: {id, name, status, previous_status, trigger}}` |
| `import.finished` | A profile import finishes | `{import: {list_id, created, updated, skipped, errors}}` |

`profile` in most topics is `{id, email, phone_number, external_id, first_name, last_name}`.
`Profile` (in `profile.created` and `profile.updated`) adds `properties`, `email_consent`,
`sms_consent`, `created_at`, `updated_at` and `last_event_at`. `message` is `{id, channel, subject,
status, campaign_id, flow_id, flow_node_id, sent_at}`.

Consent `reason` values: `created` (a new profile that arrived subscribed), `profile_update` (the
API, dashboard or an import changed it), `double_opt_in` (confirmed by email), `unsubscribe_link`,
`stop_keyword` / `start_keyword` (texts), `spam_complaint`.

Segment membership is checked when segments refresh, about every 15 minutes (every 5 for segments
that trigger flows), so `segment.entered` and `segment.left` arrive within that time. Removing a
whole list or segment doesn't send a `member_removed` / `left` event for each member.

`GET /api/webhooks/topics` lists every topic with a description. `GET /api/hooks/samples?topic=...`
returns a sample envelope for any topic.

## The envelope

Every event is one JSON object:

```json
{
  "id": "0193a5b4-8c30-7a77-8e33-4b5c6d7e8f90",
  "topic": "event.tracked",
  "created_at": "2026-11-17T18:04:05.123Z",
  "account_id": "0193a5b4-0000-7000-8000-000000000001",
  "api_version": "2026-11-17",
  "data": { "event": { "metric": "Placed Order", "value": 38.0, "...": "..." }, "profile": { "email": "jane@example.com", "...": "..." } }
}
```

- `id` is the event's id. It stays the same on every retry and resend, so use it to drop duplicates.
- Test events from the "Send test event" button also have `"test": true`.
- With `batch_size` 1 a request body is one envelope. With a larger `batch_size` (the default is 50)
  the body is always a JSON array of up to that many envelopes, even when there's only one.
- Ordering is best-effort: events usually arrive in the order they happened, but retries and
  batching can reorder them. Use `created_at` (and the timestamps inside `data`) to order them.

## Verifying the signature

Each endpoint has a signing secret (`whsec_...`), shown once when you create it or rotate it.
Every request carries:

| Header | Value |
| --- | --- |
| `DumbMail-Webhook-Id` | A new id for this request (not the event id) |
| `DumbMail-Webhook-Timestamp` | Unix time in seconds when it was signed |
| `DumbMail-Webhook-Signature` | `v1=<hex>`; during a rotation `v1=<hex>,v1=<hex>` |
| `DumbMail-Webhook-Topic` | The topic, when the body is a single envelope |

The signature is the hex HMAC-SHA256 of `{timestamp}.{raw request body}` keyed with the secret
(the whole string, including `whsec_`). To check a request:

1. Read the raw body bytes before parsing JSON.
2. Compute `HMAC_SHA256(secret, timestamp + "." + body)` as lowercase hex.
3. Accept it if any `v1=` value in `DumbMail-Webhook-Signature` matches, compared in constant time.
4. Reject timestamps more than 5 minutes from your clock, so a captured request can't be replayed later.

**Node.js**

```js
import crypto from "node:crypto";

// rawBody: the request body as a Buffer (e.g. express.raw({ type: "application/json" })).
export function verifyDumbMail(rawBody, headers, secret, toleranceSeconds = 300) {
  const ts = headers["dumbmail-webhook-timestamp"];
  if (!/^\d+$/.test(ts ?? "") || Math.abs(Date.now() / 1000 - Number(ts)) > toleranceSeconds) return false;
  const expected = crypto.createHmac("sha256", secret).update(`${ts}.`).update(rawBody).digest("hex");
  return (headers["dumbmail-webhook-signature"] ?? "")
    .split(",")
    .map((s) => s.trim())
    .filter((s) => s.startsWith("v1="))
    .some((s) => s.length === expected.length + 3 && crypto.timingSafeEqual(Buffer.from(s.slice(3)), Buffer.from(expected)));
}
```

**Python**

```python
import hashlib, hmac, time

def verify_dumbmail(raw_body: bytes, headers, secret: str, tolerance: int = 300) -> bool:
    ts = headers.get("DumbMail-Webhook-Timestamp", "")
    if not ts.isdigit() or abs(time.time() - int(ts)) > tolerance:
        return False
    expected = hmac.new(secret.encode(), ts.encode() + b"." + raw_body, hashlib.sha256).hexdigest()
    parts = [p.strip() for p in headers.get("DumbMail-Webhook-Signature", "").split(",")]
    return any(hmac.compare_digest(p[3:], expected) for p in parts if p.startswith("v1="))
```

**Ruby**

```ruby
require "openssl"

def verify_dumbmail(raw_body, headers, secret, tolerance = 300)
  ts = headers["DumbMail-Webhook-Timestamp"].to_s
  return false unless ts.match?(/\A\d+\z/) && (Time.now.to_i - ts.to_i).abs <= tolerance
  expected = OpenSSL::HMAC.hexdigest("SHA256", secret, "#{ts}.#{raw_body}")
  headers["DumbMail-Webhook-Signature"].to_s.split(",").map(&:strip).any? do |part|
    sig = part.delete_prefix("v1=")
    part.start_with?("v1=") && sig.bytesize == expected.bytesize && OpenSSL.fixed_length_secure_compare(sig, expected)
  end
end
```

**PHP**

```php
function verify_dumbmail(string $rawBody, string $timestamp, string $signature, string $secret, int $tolerance = 300): bool {
    if (!ctype_digit($timestamp) || abs(time() - (int) $timestamp) > $tolerance) {
        return false;
    }
    $expected = hash_hmac('sha256', $timestamp . '.' . $rawBody, $secret);
    foreach (explode(',', $signature) as $part) {
        $part = trim($part);
        if (str_starts_with($part, 'v1=') && hash_equals($expected, substr($part, 3))) {
            return true;
        }
    }
    return false;
}

// $ok = verify_dumbmail(file_get_contents('php://input'), $_SERVER['HTTP_DUMBMAIL_WEBHOOK_TIMESTAMP'] ?? '',
//                       $_SERVER['HTTP_DUMBMAIL_WEBHOOK_SIGNATURE'] ?? '', getenv('DUMBMAIL_WEBHOOK_SECRET'));
```

**Rust** (crates `hmac`, `sha2`, `hex`)

```rust
use hmac::{Hmac, Mac};
use sha2::Sha256;

fn verify_dumbmail(raw_body: &[u8], timestamp: &str, signature: &str, secret: &str) -> bool {
    let Ok(ts) = timestamp.parse::<i64>() else { return false };
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
    if (now - ts).abs() > 300 {
        return false;
    }
    signature.split(',').filter_map(|p| p.trim().strip_prefix("v1=")).filter_map(|h| hex::decode(h).ok()).any(|sig| {
        let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("any key length works");
        mac.update(timestamp.as_bytes());
        mac.update(b".");
        mac.update(raw_body);
        mac.verify_slice(&sig).is_ok()
    })
}
```

**Rotating a secret.** `POST /api/webhooks/:id/rotate-secret` (or "Rotate secret" in the dashboard)
returns a new secret once. For the next 24 hours requests are signed with both the new and the old
secret, so you can switch your receiver over without dropping anything.

## Delivery, retries and limits

- Deliveries are queued and sent in the background; they never slow down sending or anything else.
  Each endpoint is sent to by one worker at a time, so a slow or broken endpoint holds up only itself.
- A request succeeds when your endpoint answers 2xx within 10 seconds. Redirects count as failures
  (use the final URL). Only the first 1 KB of your response is kept for the log.
- A failed delivery is retried with exponential back-off (30 seconds, doubling, at most 2 hours
  apart) for 24 hours, then marked `failed`. After a failure the whole endpoint backs off too
  (30 seconds, doubling, at most an hour), so a broken endpoint isn't hit with every new event.
- An endpoint that fails every delivery for 3 days is turned off and the account owner gets an
  email. Turn it back on in the dashboard (or `PATCH {enabled: true}`) and resend what failed.
- An endpoint answering **410 Gone** is telling DumbMail to stop: REST hooks (Zapier, Make, API)
  are deleted, dashboard endpoints are turned off.
- `rate_per_second` (1-100, default 10) caps requests per second to the endpoint; `batch_size`
  (1-100, default 50, REST hooks 1) caps events per request.
- `filter` sends only events whose `data` matches every condition: `[{field, op, value}]`, where
  `field` is a dotted path into `data` (`event.value`, `event.properties.total`, `list.id`,
  `profile.email`) and `op` is a segment property operator (`eq`, `neq`, `contains`, `gt`, `gte`,
  `lt`, `lte`, `in`, `exists`, ...).
- Changes to endpoints take effect within 30 seconds.
- Up to 100 endpoints per account. Delivery logs, with payloads, are kept for 14 days.

## Delivery log, resends and tests

- `GET /api/webhooks/:id/deliveries?status=&topic=&limit=&offset=` – newest first:
  `{data: {id, event_id, topic, status: "pending"|"delivered"|"failed", test, attempts, response_code,
  error, duration_ms, next_attempt_at, last_attempt_at, delivered_at, created_at}[], total}`.
- `GET /api/webhooks/:id/deliveries/:delivery_id` – one delivery with its `payload` and `response_body`.
- `POST /api/webhooks/:id/resend` `{delivery_ids?: string[], since?, until?, status?: "failed"|"delivered"|"all"}`
  – queue earlier deliveries again (up to 10,000; `failed` by default), right away even if the
  endpoint was backing off. Returns `{resent}`.
- `POST /api/webhooks/:id/test` `{topic?}` – send one sample event now (marked `"test": true`) and
  return `{ok, delivery, response_body}`. It's tried once and shows in the log.

## Flow webhook steps

A flow's webhook step is signed the same way, with one secret per account (not per endpoint).
The request body is unchanged (`{flow, node_id, profile, event, body}`); a `GET` step signs an
empty body. The secret is created the first time a flow webhook runs.

- `GET /api/webhooks/flow-secret` – `{secret_hint, created_at}` (`null` until one exists).
- `POST /api/webhooks/flow-secret/rotate` – a new secret, shown once. The old one stops working
  right away, so update your receiver at the same time.

## API

| Method | Path | Body | Returns |
| --- | --- | --- | --- |
| GET | `/api/webhooks/topics` | – | `{data: {topic, group, description}[]}` |
| GET | `/api/webhooks` | – | `{data: Endpoint[], total}` |
| POST | `/api/webhooks` | `{url, topics, metrics?, filter?, description?, enabled?, batch_size?, rate_per_second?}` | `Endpoint` plus `secret` (shown once), 201 |
| GET/PATCH/DELETE | `/api/webhooks/:id` | partial body as above; `filter: null` removes it | `Endpoint` |
| POST | `/api/webhooks/:id/rotate-secret` | – | `{secret, secret_hint, previous_secret_valid_until}` |

`Endpoint` is `{id, url, description, topics, metrics, filter, enabled, status: "active"|"failing"|"disabled",
source: "dashboard"|"api"|"zapier"|"make", batch_size, rate_per_second, secret_hint,
previous_secret_valid_until, consecutive_failures, failing_since, next_retry_at, disabled_reason,
last_success_at, last_failure_at, created_at, updated_at, stats: {pending, delivered_24h, failed_24h}}`.

## REST hooks (Zapier, Make and your own code)

Integration platforms subscribe a URL to one topic with an API key (`Authorization: Bearer pl_sk_...`):

| Method | Path | Body | Returns |
| --- | --- | --- | --- |
| GET | `/api/hooks/me` | – | `{account: {id, name}}`, for testing the key |
| POST | `/api/hooks` | `{url (or hookUrl / target_url), topic, metric?, filter?, source?: "zapier"|"make"|"api"}` | `{id, url, topic, metric, secret}`, 201 |
| DELETE | `/api/hooks/:id` | – | `{ok: true}` |
| GET | `/api/hooks/samples?topic=&metric=` | – | an array of sample envelopes; for `event.tracked` with a metric, the account's latest real events of that metric when there are any |

REST hooks get one envelope per request (`batch_size` 1) and are signed like any endpoint. They
show up in Settings → Webhooks with their source and can be managed there.

The actions Zapier and Make need are the regular API: `POST /api/profiles` (create or update a
profile; send `email_consent: "unsubscribed"` to unsubscribe), `POST /api/events` (track an event),
`POST /api/lists/:id/members` `{emails}` (add to a list), `DELETE /api/lists/:id/members`
`{profile_ids}` (remove; look the profile up with `GET /api/profiles?q=<email>`) and `GET /api/lists`.
The apps in `connectors/zapier` and `connectors/make` in the DumbMail repository use them.

## MCP tools

`list_webhook_topics`, `list_webhooks`, `create_webhook`, `update_webhook`, `delete_webhook`
(needs `confirm: true`), `rotate_webhook_secret`, `send_test_webhook`, `list_webhook_deliveries`,
`resend_webhook_deliveries`, `get_flow_webhook_secret`, `rotate_flow_webhook_secret`.
