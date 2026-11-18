# Square recipe

Checked against Square's documentation on 2026-11-17; not yet run against a live account.

- **Signature.** `x-square-hmacsha256-signature` is a base64 HMAC-SHA256, keyed with the subscription's signature key, of the notification URL followed by the raw body. DumbMail signs its own webhook URL (as shown on the integration) plus the body, so the URL in Square must match it exactly. Checked with Square's own example (key `asdf1234`, URL `https://example.com/webhook`, body `{"hello":"world"}` → `2kRE5qRU2tR+tBGlDwMEw2avJ7QM4ikPYD/PJ3bd9Og=`). Source: [Verify and validate an event notification](https://developer.squareup.com/docs/webhooks/step3validate).
- **Body.** `{"merchant_id", "type", "event_id", "created_at", "data": {"type", "id", "object": {"<type>": …}}}`. `event_id` is "the unique ID of the event, which is used for idempotency support", so it's the message id. Sources: [payment.updated](https://developer.squareup.com/reference/square/payments-api/webhooks/payment.updated), [refund.updated](https://developer.squareup.com/reference/square/refunds-api/webhooks/refund.updated), [customer.created](https://developer.squareup.com/reference/square/customers-api/webhooks/customer.created).
- **Orders.** A Payment's `status` is `APPROVED`, `PENDING`, `COMPLETED`, `CANCELED` or `FAILED`. A `COMPLETED` payment for an order fetches the order, `GET /v2/orders/{order_id}` (`{"order": …}`, Bearer access token with ORDERS_READ), and the order counts once, by its id, at its `total_money`, when `net_amount_due_money` ("the net amount of money due on the order") is zero: an order paid in several parts counts once, in full. A completed payment with no order counts on its own, once. The buyer is `buyer_email_address`, else the Square `customer_id`. Sources: [Payment](https://developer.squareup.com/reference/square/objects/Payment), [Retrieve order](https://developer.squareup.com/reference/square/orders-api/retrieve-order), [Order](https://developer.squareup.com/reference/square/objects/Order).
- **Refunds.** Counted once per refund id when its `status` is `COMPLETED`, with its own `amount_money`. A refund names its `payment_id`, which is saved on the buyer's profile from the payment (from a payment that names only a customer too), and the refund waits (retry after 1, 5, 30 and 120 minutes) until that payment has arrived. Source: refund.updated.
- **Money.** Amounts are in the smallest unit of the currency, so they're divided by 100 except for zero-decimal currencies (JPY and others). Source: [Working with monetary amounts](https://developer.squareup.com/docs/build-basics/common-data-types/working-with-monetary-amounts).
- **Consent.** Square's customer has `preferences.email_unsubscribed` ("whether the customer has unsubscribed from marketing campaign emails") but no opt-in. `true` suppresses the address from marketing; nobody is subscribed. Source: customer.created.
- **Orders API.** The `order.*` webhooks carry only an id and state, so the recipe records orders from payments (each Placed Order has the `order_id`).

## Removed before anything is stored

Card, bank, wallet and buy-now-pay-later details, card fingerprints, birthdays and addresses, at any depth.

## What a live check should look at first

- That the signature matches with the URL as DumbMail shows it (and still does after rotating the URL in DumbMail, once the new URL is in Square).
- How often a completed payment carries `buyer_email_address`; card-present payments may only have a `customer_id`.
- An order paid in two tenders: `net_amount_due_money` reaching zero with the last one. The access token is for production (`connect.squareup.com`); a sandbox account uses `connect.squareupsandbox.com` in the Pull tab.
