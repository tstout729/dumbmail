# WooCommerce recipe

Checked against WooCommerce's documentation on 2026-11-17; not yet run against a live store.

What the script assumes, and the WooCommerce page each assumption comes from ([REST API docs](https://woocommerce.github.io/woocommerce-rest-api-docs/), source in [woocommerce-rest-api-docs](https://github.com/woocommerce/woocommerce-rest-api-docs)).

- **Signature.** `X-WC-Webhook-Signature` is "a base64 encoded HMAC-SHA256 hash of the payload", keyed with the webhook's `secret`. Source: [Webhooks → Delivery/payload](https://woocommerce.github.io/woocommerce-rest-api-docs/#webhooks).
- **Headers.** Each delivery carries `X-WC-Webhook-Topic` (for example `order.updated`), `X-WC-Webhook-Resource`, `X-WC-Webhook-Event`, `X-WC-Webhook-ID` and `X-WC-Webhook-Delivery-ID`. The script reads the topic; the delivery id is the message id, so a second delivery with the same id is ignored. Same source.
- **Payload.** "For API resources (coupons, customers, orders, products), the response is exactly the same as if requested via the REST API." So order webhooks carry the [order object](https://woocommerce.github.io/woocommerce-rest-api-docs/#order-properties) and customer webhooks the [customer object](https://woocommerce.github.io/woocommerce-rest-api-docs/#customer-properties).
- **Paid.** An order's `status` is one of `pending`, `processing`, `on-hold`, `completed`, `cancelled`, `refunded`, `failed`, `trash`, and `date_paid_gmt` is when it was paid. Placed Order is recorded when `date_paid_gmt` is set or the status is `processing` or `completed`, once per order. Pending, on-hold and failed orders aren't paid. An order whose `total` is 0 isn't counted. Source: order properties.
- **Money.** Amounts are strings with two decimals, in the order's `currency` (ISO code), which goes on every event. Source: [Introduction](https://woocommerce.github.io/woocommerce-rest-api-docs/#introduction) and order properties.
- **Times.** `*_gmt` fields are GMT without a zone; the script adds `Z`.
- **Refunds.** The order's `refunds` list has each refund's `id`, `reason` and `total` (a negative string in the documented example, which also names the reason field `refund`; the script reads both). Each refund is recorded once by its id, with the positive amount, dated by the order's `date_modified_gmt` (refund entries have no date). WooCommerce lists a refund only after it's created; with `api_refund` the gateway refund must succeed first. Sources: order refunds properties and [Order refunds](https://woocommerce.github.io/woocommerce-rest-api-docs/#order-refunds).
- **Cancelled.** Recorded once, only for an order that had been paid (WooCommerce also cancels unpaid orders when they time out; those were never orders).
- **Consent.** WooCommerce core has no marketing opt-in field on orders or customers, so nobody is subscribed and everyone this adds is held back from marketing.
- **Phones.** WooCommerce stores phones as typed. Only numbers already in international form (`+…`) or 10-digit US and Canadian numbers (with the billing country) are kept.
- **Ping.** Saving a webhook makes WooCommerce send a test request with `webhook_id`; it's logged and ignored.
- **Import.** `GET /wp-json/wc/v3/orders` with `orderby=modified`, `order=asc`, `modified_after` and `dates_are_gmt=true`, paged with `per_page` and `offset`; authentication is HTTP Basic with the consumer key and secret over HTTPS. Each page starts just before the newest change already read and skips the orders already read there, so orders changed in the same second are neither missed nor read twice. Sources: [List all orders](https://woocommerce.github.io/woocommerce-rest-api-docs/#list-all-orders), [Authentication over HTTPS](https://woocommerce.github.io/woocommerce-rest-api-docs/#authentication-over-https) and [Pagination](https://woocommerce.github.io/woocommerce-rest-api-docs/#pagination). The store's address is part of the URL, so the Pull tab needs it before an import.

## Removed before anything is stored

`customer_ip_address`, `customer_user_agent`, `meta_data` (payment plugins keep tokens there), `payment_url` and `transaction_id`, at any depth.

## What a live check should look at first

- That the signature matches on a real delivery (the secret is used as typed).
- What a refund looks like on a real store: the refund entry's field names and whether a gateway refund that fails still appears in `refunds`.
- Whether `modified_after` is exclusive and second-precise on the store's WooCommerce version.
