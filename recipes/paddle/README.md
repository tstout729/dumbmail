# Paddle recipe

Paddle Billing (not Paddle Classic). Picked over Chargebee because Paddle signs its webhooks with an HMAC and documents it precisely; Chargebee's webhooks rely on basic auth. Checked against Paddle's documentation on 2026-11-17; not yet run against a live account.

- **Signature.** `Paddle-Signature: ts=<unix>;h1=<hex>`, where `h1` is an HMAC-SHA256 of `"<ts>:<raw body>"` keyed with the destination's secret key (`pdl_ntfset_…`, used as is). More than one `h1` can appear while a secret rotates; any match passes. Paddle's SDKs allow 5 seconds of clock difference; DumbMail allows 5 minutes. Source: [Verify webhook signatures](https://developer.paddle.com/webhooks/signature-verification).
- **Body.** `{"event_id", "event_type", "occurred_at", "notification_id", "data"}`. `event_id` is the message id, so a resend is ignored. Source: [Webhooks overview](https://developer.paddle.com/webhooks/overview).
- **Orders.** `transaction.completed`, once per transaction id, valued at `details.totals.grand_total` in `currency_code`. Amounts are strings in the lowest unit (cents), so they're divided by 100, except CLP, JPY and KRW, which Paddle lists with no decimals ([Supported currencies](https://developer.paddle.com/concepts/sell/supported-currencies)). Source: [transaction.completed](https://developer.paddle.com/webhooks/transactions/transaction-completed).
- **Refunds.** An adjustment with `action: refund` "must be approved by Paddle in most cases"; its `status` is `pending_approval`, `approved`, `rejected` or `reversed`. Recorded once per adjustment id when `approved`, with `totals.total`. Source: [adjustment.updated](https://developer.paddle.com/webhooks/adjustments/adjustment-updated).
- **Subscriptions.** Started once per subscription (from `subscription.created` when `active` or `trialing`, or `subscription.activated`), dated by `started_at`; Cancelled from `subscription.canceled`, dated by `canceled_at`. Sources: [subscription.created](https://developer.paddle.com/webhooks/subscriptions/subscription-created) and [subscription.canceled](https://developer.paddle.com/webhooks/subscriptions/subscription-canceled).
- **Who.** Transactions, adjustments and subscriptions carry only `customer_id`; `customer.created` and `customer.updated` carry the email. The Paddle customer id is saved as a key, and the other events wait (retry after 1, 5, 30 and 120 minutes) until the customer is known.
- **Consent.** `marketing_consent` is "`false` unless customers check the marketing consent box" at checkout, and `customer.updated` reports later changes. `true` subscribes that customer's email, with the source "Paddle Checkout marketing consent box". A change to `false` unsubscribes them only when their latest opt-in came from Paddle; consent they gave anywhere else is left alone. Changes apply in Paddle's order (each carries the customer's `updated_at`), so an older change arriving late never undoes a newer one. Source: [customer.created](https://developer.paddle.com/webhooks/customers/customer-created).

## Removed before anything is stored

`payments` (card brand, last 4, cardholder name), `method_details`, `card`, `address` and `business`, at any depth.

## What a live check should look at first

- That `h1` matches on a real delivery and Paddle's retries arrive within 5 minutes of their `ts`.
- That `customer.created` usually arrives before or soon after `transaction.completed` (the order waits for it).
