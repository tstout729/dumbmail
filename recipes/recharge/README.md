# Recharge recipe

Checked against Recharge's documentation on 2026-11-17; not yet run against a live account.

- **Signature.** Each webhook carries `X-Recharge-Webhook-Signature: t=<epoch>,v1=<hex>`, an HMAC-SHA256 of `"<t>.<raw body>"` keyed with the API Client Secret, and Recharge says to reject a timestamp more than 48 hours off. DumbMail checks it with the `t=…,v1=…` format and a 24-hour tolerance (its maximum). The legacy `X-Recharge-Hmac-Sha256` header (a plain SHA-256 of secret + body) isn't used. Source: [Webhook validation](https://developer.rechargepayments.com/v2/webhooks_validation).
- **Body.** "Callback payloads are identical to Recharge's REST API payloads": `{"subscription": …}`, `{"charge": …}`, `{"customer": …}`. Source: [Using webhooks](https://docs.getrecharge.com/docs/webhooks-overview) and [Webhook payload examples](https://docs.getrecharge.com/docs/webhook-payload-examples).
- **Which topic.** The body doesn't say which topic sent it, and the docs name no topic header. So each webhook is created with its topic in the URL (`?topic=charge/paid`); Recharge signs only the body. A webhook without one is logged and ignored. Source: [Webhook endpoints](https://developer.rechargepayments.com/2021-11/webhooks_endpoints) (webhooks are created with an `address` and a `topic`).
- **Topics.** `subscription/created`, `subscription/activated`, `subscription/cancelled`, `subscription/skipped`, `charge/upcoming` ("X days before the scheduled charge", 3 by default), `charge/paid` (not for the checkout itself), `charge/failed` (every failed attempt), `customer/created`, `customer/updated`. Source: [Webhooks explained](https://developer.rechargepayments.com/2021-11/webhooks_explained).
- **Who.** Charges and customers carry the email (`charge.customer.email`, `customer.email`); subscriptions carry only `customer_id`. The Recharge customer id is saved as a key, and a subscription event waits (retry after 1, 5, 30 and 120 minutes) until that customer is known. Sources: the [subscription](https://developer.rechargepayments.com/2021-11/subscriptions), [charge](https://developer.rechargepayments.com/2021-11/charges) and [customer](https://developer.rechargepayments.com/2021-11/customers) objects.
- **Resends.** Recharge retries a webhook up to 20 times over 48 hours and documents no delivery id, so duplicates are caught by each event's `unique_id`: the subscription or charge id, plus Recharge's own time for things that can happen more than once (reactivated, cancelled, skipped, a failed attempt, an upcoming date). Source: Using webhooks.
- **Money.** `total_price` (a string) in the charge's `currency`. A charge with a total of 0 isn't recorded as paid.
- **Consent.** Recharge's customer object has no marketing opt-in, so nobody is subscribed and everyone this adds is held back from marketing.

## Removed before anything is stored

`client_details` (browser IP and user agent), `payment_methods`, `payment_method`, `billing_address` and `analytics_data`, at any depth.

## What a live check should look at first

- That the `t=…,v1=…` signature matches a real delivery, and whether Recharge signs again on each retry (the 24-hour tolerance assumes a retry within a day, or a fresh signature).
- That a webhook address with `?topic=…` is accepted by Recharge and delivered with the query intact.
- What `subscription/skipped` carries (the docs show the subscription; it may include the charge).
