# WooCommerce recipe

Checked against WooCommerce's documentation on 2026-11-17; not yet run against a live store.

What the script assumes, and the WooCommerce page each assumption comes from ([REST API docs](https://woocommerce.github.io/woocommerce-rest-api-docs/), source in [woocommerce-rest-api-docs](https://github.com/woocommerce/woocommerce-rest-api-docs)).

- **Signature.** `X-WC-Webhook-Signature` is "a base64 encoded HMAC-SHA256 hash of the payload", keyed with the webhook's `secret`. Source: [Webhooks → Delivery/payload](https://woocommerce.github.io/woocommerce-rest-api-docs/#webhooks).
- **Signature window.** WooCommerce's signature covers only the body and sends no timestamp, so there's no time window; a resend is recognized by `X-WC-Webhook-Delivery-ID`, and every event's id comes from the order or refund id. Source: [Webhooks](https://woocommerce.github.io/woocommerce-rest-api-docs/#webhooks).
- **Headers.** Each delivery carries `X-WC-Webhook-Topic` (for example `order.updated`), `X-WC-Webhook-Resource`, `X-WC-Webhook-Event`, `X-WC-Webhook-ID` and `X-WC-Webhook-Delivery-ID`. The script reads the topic; the delivery id is the message id, so a second delivery with the same id is ignored. Same source.
- **Payload.** "For API resources (coupons, customers, orders, products), the response is exactly the same as if requested via the REST API." So order webhooks carry the [order object](https://woocommerce.github.io/woocommerce-rest-api-docs/#order-properties) and customer webhooks the [customer object](https://woocommerce.github.io/woocommerce-rest-api-docs/#customer-properties).
- **Paid.** An order's `status` is one of `pending`, `processing`, `on-hold`, `completed`, `cancelled`, `refunded`, `failed`, `trash`, and `date_paid_gmt` is when it was paid. Placed Order is recorded when `date_paid_gmt` is set or the status is `processing` or `completed`, once per order. Pending, on-hold and failed orders aren't paid. An order whose `total` is 0 isn't counted. Source: order properties.
- **Money.** Amounts are strings with two decimals, in the order's `currency` (ISO code), which goes on every event. Source: [Introduction](https://woocommerce.github.io/woocommerce-rest-api-docs/#introduction) and order properties.
- **Times.** `*_gmt` fields are GMT without a zone; the script adds `Z`.
- **Refunds.** An order's `refunds` list is only a summary (`id`, `reason`, `total`, no date), so when an order lists refunds DumbMail fetches the refund records, `GET /wp-json/wc/v3/orders/{id}/refunds`, each with its own `amount`, `date_created_gmt` and `refunded_payment` ("if the payment was refunded via the API"). Each refund is recorded once by its id, with its own amount and date; one recorded by the merchant without the payment gateway moving the money (`refunded_payment: false`) still counts as a refund and carries `gateway: false`. This needs the REST API key (see setup). A history import doesn't import refunds. Sources: [Order refunds](https://woocommerce.github.io/woocommerce-rest-api-docs/#order-refund-properties).
- **Cancelled.** A paid order that's cancelled is Cancelled Order, once. An order that was never paid and is cancelled (WooCommerce also cancels unpaid orders when they time out) is Unpaid Order Cancelled, kept apart from orders.
- **Consent.** WooCommerce core has no marketing opt-in field on orders or customers, so nobody is subscribed and everyone this adds is held back from marketing.
- **Phones.** WooCommerce stores phones as typed. Only numbers already in international form (`+…`) or 10-digit US and Canadian numbers (with the billing country) are kept.
- **Ping.** Saving a webhook makes WooCommerce send a test request with `webhook_id`; it's logged and ignored.
- **Import.** `GET /wp-json/wc/v3/orders` with `orderby=modified`, `order=asc`, `dates_are_gmt=true`, and `modified_after` / `modified_before` windows (`per_page=100`). There are no offsets into a list that keeps changing: a full page means the window was too wide, so it's halved and read again; a window of a second or two that's still full is paged through; each next window starts a second before the last ended, so an order changed at the boundary is never missed, and orders read twice are recorded once by their ids. Authentication is HTTP Basic with the consumer key and secret over HTTPS. Sources: [List all orders](https://woocommerce.github.io/woocommerce-rest-api-docs/#list-all-orders), [Authentication over HTTPS](https://woocommerce.github.io/woocommerce-rest-api-docs/#authentication-over-https). The store's address is part of the URL, so the Pull tab needs it.

## Removed before anything is stored

`customer_ip_address`, `customer_user_agent`, `meta_data` (payment plugins keep tokens there), `payment_url` and `transaction_id`, at any depth.

## What a live check should look at first

- That the signature matches on a real delivery (the secret is used as typed).
- A manual refund and a gateway refund on a real store: `refunded_payment` false and true.
- Whether `modified_after` and `modified_before` are exclusive and second-precise on the store's WooCommerce version, and a store installed under a sub-path (the refund fetch expects `/wp-json/` at the root).
