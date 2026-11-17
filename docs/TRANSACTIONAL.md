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
| `to` | One recipient: an email address, or `{email, name?, first_name?, last_name?, external_id?, properties?}`. Lists and several addresses are refused. The person's profile is created or updated (names and properties only; consent never changes). |
| `template_id` | A transactional template (see below). Give this or `content`. |
| `content` | `{subject, preheader?, blocks? \| html? \| text?}`. Blocks are the same as campaigns (see `dumbmail://docs/email`). Raw `html` up to 512 KB; plain `text` up to 128 KB. With only `text`, the HTML part is made from it. |
| `subject` | Replaces the template's or content's subject. |
| `variables` | Values for `{{ tags }}` (up to 64 KB). Profile fields such as `{{ first_name }}` work too, and `{{ variables.x }}` reaches every variable. |
| `from_email`, `from_name`, `reply_to` | Default to the account's sender. `from_email` must be on a verified sending domain. |
| `idempotency_key` | Up to 255 characters. The same key within 24 hours returns the first result and sends nothing. Reusing a key for a different email is refused (409). After 24 hours the key can be used again. |
| `metadata` | Your own fields (up to 50 keys, 8 KB), kept with the send and on the profile's timeline event. |
| `track_clicks` | Rewrite links for click tracking. Default `false`. |
| `track_opens` | Add the open pixel. Default `true`. |
| `address_footer` | Add the account's name and postal address. Default `false`. |

The response is the send:

```json
{
  "id": "0193f3c0-...", "to": "ann@example.com", "subject": "Order 1042 confirmed",
  "status": "sent", "error": null, "message_id": "0193f3c0-...", "profile_id": "0193...",
  "idempotency_key": "order-1042-receipt", "metadata": {"order_id": "1042"},
  "created_at": "2026-11-17T18:04:11Z", "sent_at": "2026-11-17T18:04:11Z", "replayed": false
}
```

`status` is one of `queued` (the provider was busy; it retries on its own), `sent`, `delivered`, `opened`, `clicked`, `bounced`, `complained`, `skipped` (the address can't get email; `error` says why) or `failed`. `replayed: true` means the idempotency key matched an earlier send.

Errors use the API's usual shape (`{"error": {"code", "message"}}`): 400 for a bad request or content that reads like marketing, 402 when a cap is reached or the balance can't cover the email, 409 when sending is paused or a key was reused for a different email.

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

Retry on network errors and 5xx with the same `idempotency_key`; it never sends twice.

## Templates

A template is sent through this API only if it was created as transactional:

```sh
curl https://dumbmail.ai/api/transactional/templates \
  -H "Authorization: Bearer $DUMBMAIL_API_KEY" -H 'Content-Type: application/json' \
  -d '{"name": "Receipt", "content": {"subject": "Order {{ order_id }} confirmed", "blocks": [{"type": "text", "html": "<p>Thanks, {{ first_name|default(\"there\") }}. Total: {{ total }}</p>"}]}}'
```

`GET /api/transactional/templates` lists them with any review warnings. Edit one like any template (`PATCH /api/templates/{id}`); it's checked again every time it's sent.

## The log

- `GET /api/transactional/sends?q=&status=&limit=&offset=`: newest first. `q` searches the recipient and subject; `status` filters by the statuses above; `limit` up to 200.
- `GET /api/transactional/sends/{id}`: one send with the original request.
- `POST /api/transactional/sends/{id}/resend`: send it again to the same person, as a new send.
- Each email also appears on the person's profile as a "Received Transactional Email" event, followed by "Opened Email" and "Clicked Email" events.

The dashboard's Transactional page shows the same log with search and resend.

## Limits and review

Transactional email skips marketing consent, so these rules keep marketing out of it:

- **Content review.** Templates are checked when they're created and every email when it's sent. Two or more promotional phrases (such as "20% off", "shop now", "limited time", "use code", "exclusive offer"), or a marketing unsubscribe link, block the email with a 400. One phrase is allowed and shows as a warning. Prices, totals and discounts on a receipt are fine.
- **One recipient per request.**
- **Caps.** 120 emails a minute and 5,000 in any 24 hours per account. `GET /api/transactional/settings` shows the caps, how much is used and whether sending is paused. Contact support to raise them.
- **Automatic pause.** If an account suddenly sends far more than usual (500 or more in an hour and ten times its usual hourly volume), or a jump of mostly people with no other activity (300 or more in 24 hours, five times a usual day, 90% to people who never did anything else), transactional email is paused for the account and the operator is alerted. Campaigns and flows keep working. Support lifts the pause after a look.
- **Bounces and complaints** from transactional email count toward the account's automatic email pause, like any email.

Operators (with `ADMIN_TOKEN`) lift a pause with `POST /admin/transactional/unpause {"account_id"}` and set caps with `POST /admin/transactional/limits {"account_id", "per_minute_limit", "daily_limit"}` (`null` returns a cap to its default).

## MCP

- `send_transactional_email`: without `confirm: true` it renders the email and says whether it would send (nothing is sent or saved); add `include_html: true` to see the HTML. With `confirm: true` it sends.
- `list_transactional_sends`, `get_transactional_send`
- `resend_transactional_email` (`confirm: true` to send)
- `create_transactional_template`, `list_transactional_templates`
- `get_transactional_settings`
