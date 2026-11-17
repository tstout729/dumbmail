# Transactional email API

Send email a person triggered: receipts, password resets, shipping notices, sign-in codes. One request sends one email to one person.

Transactional email is different from marketing:

- It goes out whether or not the person agreed to marketing email, because they asked for it.
- It's never sent to an address that hard bounced or marked an email as spam. Those sends come back with `status: "skipped"`.
- It has no unsubscribe link or footer. Add your name and postal address with `address_footer: true`.
- The warm-up limit doesn't hold it back; only the email provider's sending rate does.
- Opens and deliveries are tracked. Click tracking is off unless you ask for it (`track_clicks: true`), so links such as password resets go straight to your site.
- It's billed like any other email, and it stops when email sending is paused for the account.
- Smart sending, campaign attribution and best-time history ignore it.

Because it skips marketing consent, it can't carry marketing. See [Limits and review](#limits-and-review).

Transactional SMS isn't available.

## Turn it on

Transactional email is off for every account until the account owner turns it on in Settings → Transactional, signed in to the dashboard, and confirms it's only for email a person triggered. Until then every send is refused with 403. Anyone on the account can turn it off again (`PATCH /api/transactional/settings {"enabled": false}`).

- **Self-hosted** (`HOSTED=false`): once it's on, everything below is allowed.
- **dumbmail.ai**: it starts in **restricted mode**. Only saved transactional templates can be sent, as saved: `template_id`, no inline `content` and no `subject` override, and each variable at most 500 characters (4 KB for all of them). Up to 200 emails a day (operators can change the default with `TRANSACTIONAL_RESTRICTED_DAILY_LIMIT`), and 120 a minute. **Full mode** adds inline content and the normal caps; ask support (the address is in `GET /api/transactional/settings` as `support_email`) with a line on what you send. Operators grant it with `POST /admin/transactional/mode {"account_id", "mode": "full"}` (or `"restricted"` to take it back).

`GET /api/transactional/settings` shows whether it's on, the mode (`off`, `restricted` or `full`), the caps and how much of each is used.

## Send an email

`POST /api/transactional/send` with an API key (Settings → Developers).

```sh
curl https://dumbmail.ai/api/transactional/send \
  -H "Authorization: Bearer $DUMBMAIL_API_KEY" \
  -H 'Content-Type: application/json' \
  -d '{
    "to": {"email": "ann@example.com", "name": "Ann Lee"},
    "template_id": "0193f3b2-5c1e-7a40-9a51-2d7c4e1f8a10",
    "variables": {"order_id": "1042", "total": "$36.00"},
    "idempotency_key": "order-1042-receipt",
    "metadata": {"order_id": "1042"}
  }'
```

Or with the content inline:

```json
{
  "to": "ann@example.com",
  "content": {
    "subject": "Reset your password",
    "text": "Use this link to reset your password. It works for one hour.\n\n{{ reset_url }}"
  },
  "variables": {"reset_url": "https://shop.example.com/reset/8f2c"}
}
```

| Field | Meaning |
| --- | --- |
| `to` | One recipient: an email address, or `{email, name?, first_name?, last_name?, external_id?, properties?}`. Lists and several addresses are refused. The email goes to exactly this address, every attempt. It's linked to the profile with this email, or to a new profile with just the email and name; an existing profile is never changed (identifiers, names, properties and consent stay as they are), and other identifiers never merge or move anything. Names and properties given here fill in this email only. |
| `template_id` | A transactional template (see below). Give this or `content`. |
| `content` | Full mode only. `{subject, preheader?, blocks? \| html? \| text?}`. Blocks are the same as campaigns (see `dumbmail://docs/email`). Raw `html` up to 512 KB; plain `text` up to 128 KB. With only `text`, the HTML part is made from it. |
| `subject` | Replaces the template's or content's subject. Full mode only. The subject is stored with the send and shown in the log, so don't put secrets in it. |
| `variables` | Values for `{{ tags }}` (up to 64 KB; in restricted mode at most 500 characters each and 4 KB in all). Profile fields such as `{{ first_name }}` work too, and `{{ variables.x }}` reaches every variable. Variables are never stored in the clear or shown in the log (see [The log](#the-log)). |
| `from_email`, `from_name`, `reply_to` | Default to the content's, then the account's sender. `from_email` must be on a verified sending domain; the Reply-To that results, wherever it comes from (including the account's default reply-to address), must be on one too, or be this email's sender address. Line breaks and other control characters are refused wherever they come from. |
| `idempotency_key` | Up to 255 characters. The same key within 24 hours returns the first result and sends nothing. Reusing a key for a different email is refused (409). After 24 hours the key can be used again. |
| `metadata` | Your own fields (up to 50 keys, 8 KB), kept with the send and on the profile's timeline event. It's stored and shown as given, so don't put secrets in it. |
| `track_clicks` | Rewrite links for click tracking. Default `false`. A click on a tracked link records that it happened, when, and the site it went to (`https://shop.example.com`), never the rest of the link, so a reset token isn't kept. Leaving it off sends links straight to your site. |
| `track_opens` | Add the open pixel. Default `true`. |
| `address_footer` | Add the account's name and postal address. Default `false`. |

The response is the send:

```json
{
  "id": "0193f3c0-...", "to": "ann@example.com", "subject": "Order 1042 confirmed",
  "status": "sent", "error": null, "message_id": "0193f3c0-...", "profile_id": "0193...",
  "idempotency_key": "order-1042-receipt", "metadata": {"order_id": "1042"}, "attempts": 1,
  "created_at": "2026-11-17T18:04:11Z", "sent_at": "2026-11-17T18:04:11Z", "replayed": false
}
```

`status` is one of:

- `queued`: waiting for an attempt. The email provider was busy; it retries on its own, up to 6 attempts over about half an hour, then fails. No attempt runs before its time.
- `sending`: an attempt is handing it to the provider right now.
- `sent`, `delivered`, `opened`, `clicked`, `bounced`, `complained`.
- `skipped`: the address can't get email (`error` says why).
- `failed`: not sent (`error` says why: for example the balance ran out, or the account was paused, before a retry).
- `unknown`: possibly delivered. The hand-off to the provider ended without a clear answer (the provider didn't answer in time, the connection dropped, it had a server error, or the attempt was cut off), so it may or may not have gone out. It isn't charged, and it's never sent again automatically, so nobody gets a second password reset; send a new request if it matters. A retry with the same `idempotency_key` returns this result.

Each hand-off to the provider is a single try: the provider's client doesn't retry on its own. Each attempt rechecks the pause, the caps, the balance and the address before it goes, and the pause, whether transactional email is on and the address once more right before the hand-off. An email is charged once, when the provider accepts it. `replayed: true` means the idempotency key matched an earlier send.

Errors use the API's usual shape (`{"error": {"code", "message"}}`): 400 for a bad request or content that reads like marketing, 402 when a cap is reached or the balance can't cover the email, 403 when transactional email is off or restricted mode doesn't allow the request, 409 when sending is paused or a key was reused for a different email.

### From code

```js
const res = await fetch("https://dumbmail.ai/api/transactional/send", {
  method: "POST",
  headers: { Authorization: `Bearer ${process.env.DUMBMAIL_API_KEY}`, "Content-Type": "application/json" },
  body: JSON.stringify({
    to: { email: order.email, first_name: order.firstName },
    template_id: process.env.RECEIPT_TEMPLATE_ID,
    variables: { order_id: order.id, items: order.items, total: order.total },
    idempotency_key: `receipt-${order.id}`,
  }),
});
const send = await res.json();
if (!res.ok) throw new Error(send.error.message);
```

```python
import os, requests

r = requests.post(
    "https://dumbmail.ai/api/transactional/send",
    headers={"Authorization": f"Bearer {os.environ['DUMBMAIL_API_KEY']}"},
    json={
        "to": "ann@example.com",
        "content": {"subject": "Your order shipped", "text": "Track it here: {{ tracking_url }}"},
        "variables": {"tracking_url": "https://track.example.com/1Z999"},
        "idempotency_key": "ship-1042",
    },
    timeout=15,
)
r.raise_for_status()
print(r.json()["status"])
```

Retry on network errors and 5xx with the same `idempotency_key`; it never sends twice. A retry of an accepted request gets the first result even when a cap has filled up since.

## Templates

A template is sent through this API only if it was created as transactional:

```sh
curl https://dumbmail.ai/api/transactional/templates \
  -H "Authorization: Bearer $DUMBMAIL_API_KEY" -H 'Content-Type: application/json' \
  -d '{"name": "Receipt", "content": {"subject": "Order {{ order_id }} confirmed", "blocks": [{"type": "text", "html": "<p>Thanks, {{ first_name|default(\"there\") }}. Total: {{ total }}</p>"}]}}'
```

`GET /api/transactional/templates` lists them with what the review found. Edit one like any template (`PATCH /api/templates/{id}`): the new content is checked when you save it (an edit that reads like marketing is refused), and again every time it's sent.

## The log

- `GET /api/transactional/sends?q=&status=&limit=&offset=`: newest first. `q` searches the recipient and subject; `status` filters by the statuses above; `limit` up to 200.
- `GET /api/transactional/sends/{id}`: one send: recipient, subject, template, status, attempts, times, metadata and delivery and engagement.
- Each email also appears on the person's profile as a "Received Transactional Email" event (with its subject and your metadata), followed by "Opened Email" and "Clicked Email" events.

The log never keeps the variables or the email body: they can hold reset links and sign-in codes. While a send waits for a retry, what it needs is kept encrypted, and it's deleted as soon as the send is sent, skipped or failed, and after 24 hours at the most (a send still waiting then fails). So there's no resend: to send an email again, make a new request with fresh values, such as a new reset link.

The dashboard's Transactional page shows the same log with search.

## Limits and review

Transactional email skips marketing consent, so these rules keep marketing out of it:

- **Content review.** Templates are checked when they're created and edited, and every email as it would go out: the final subject, sender name, HTML and plain-text parts. One clearly promotional thing blocks it with a 400:
  - discount or offer language: "20% off", "$10 off", "off your next order", "promo code" / "coupon code" / "use code X at checkout", "limited time", "flash sale" and other named sales, "don't miss out", "exclusive offer", "special offer", "save up to", "offer ends soon", "free shipping on orders over", "new arrivals";
  - "shop now" / "buy now" style calls to action;
  - links to 3 or more different product or collection pages;
  - a marketing unsubscribe link.

  Prices, totals and an applied discount shown as an amount ("Discount −$4.00", "Discount code SAVE10 applied") are fine on a receipt, and so are sign-in codes ("Enter the code 482913").
- **One recipient per request.**
- **Caps.** 120 emails a minute and 5,000 in any 24 hours per account in full mode (200 a day in restricted mode). `GET /api/transactional/settings` shows the caps, how much is used and whether sending is paused. Contact support to raise them.
- **Automatic pause.** If an account suddenly sends far more than usual (500 or more in an hour and ten times its usual hourly volume), or a jump of mostly people with no other activity (300 or more in 24 hours, five times a usual day, 90% to people who never did anything else), transactional email is paused for the account and the operator is alerted. Campaigns and flows keep working. Support lifts the pause after a look.
- **Bounces and complaints** from transactional email count toward the account's automatic email pause, like any email.

Operators (with `ADMIN_TOKEN`) lift a pause with `POST /admin/transactional/unpause {"account_id"}`, set caps with `POST /admin/transactional/limits {"account_id", "per_minute_limit", "daily_limit"}` (`null` returns a cap to its default) and grant or take back full mode with `POST /admin/transactional/mode {"account_id", "mode"}`.

## MCP

- `send_transactional_email`: without `confirm: true` it renders the email and says whether it would send (nothing is sent or saved); add `include_html: true` to see the HTML. With `confirm: true` it sends.
- `list_transactional_sends`, `get_transactional_send`
- `create_transactional_template`, `list_transactional_templates`
- `get_transactional_settings` (whether it's on, the mode and the caps). Turning it on is for the account owner, in the dashboard.
