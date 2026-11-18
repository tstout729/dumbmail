# ShipStation recipe

Checked against ShipStation's documentation on 2026-11-18; not yet run against a live account.

- **Webhooks.** `POST /webhooks/subscribe` with `target_url` and `event` (`SHIP_NOTIFY`, `ITEM_SHIP_NOTIFY` and others), or in ShipStation's settings. The body is a Webhook object: `resource_url` ("Use this URL to get the resource that triggered the webhook", up to 200 characters, Basic auth) and `resource_type`. Example: `{"resource_url": "https://ssapi.shipstation.com/shipments?storeID=123456&batchId=12345678", "resource_type": "SHIP_NOTIFY"}`. Sources: [Subscribe to webhook](https://www.shipstation.com/docs/api/webhooks/subscribe/), [Webhook model](https://www.shipstation.com/docs/api/models/webhook/), [ShipStation webhooks (help center)](https://help.shipstation.com/hc/en-us/articles/360025856252-ShipStation-Webhooks).
- **Authentication.** ShipStation doesn't sign webhooks. The integration's URL is the secret, so there's no signature or window, and nothing in a webhook is trusted: the `resource_url` must be a ShipStation `/shipments` URL naming a `batchId`, and only its batch and store ids (digits) are used, to fetch the shipments with your API key.
- **Shipments.** `GET /shipments` (the batch's ids, `page`, `pageSize` up to 500; 50 here): `shipmentId`, `orderId`, `orderNumber`, `trackingNumber`, `carrierCode`, `serviceCode`, `shipDate`, `voided`, `isReturnLabel`, and `total`/`page`/`pages`. A shipment has no customer email. The first page asks for the batch's other pages (up to 40). Source: [List shipments](https://docs.shipstation.com/apis/shipstation-v1/openapi/shipments/list_shipments).
- **Orders.** Each shipped order is fetched (`GET /orders/{orderId}`) for its `customerEmail`, carrying its shipments. Source: [Get order](https://www.shipstation.com/docs/api/orders/get-order/).
- **Rate limit.** 40 requests a minute per key; a 429 carries `X-Rate-Limit-Reset` (seconds until the window resets), which DumbMail waits for before trying again. Source: [Requirements](https://www.shipstation.com/docs/api/requirements/).
- **Events.** Order Shipped, once per shipment id, on the order's customer. Voided labels and return labels aren't shipments.
- **Consent.** ShipStation's order has no marketing opt-in field. Customers are held back from marketing; the suggested shipping email is a service message.

## Removed before anything is stored

An allow-list keeps the webhook's two fields, the shipments' ids, numbers, tracking, carrier, service, ship date and void/return flags, the paging counts, and the order's id, number and customer email. Ship-to and bill-to addresses, items, labels, notes and gift messages are dropped.

## What a live check should look at first

- The query keys real `resource_url`s carry (`storeID` or `storeId`, `batchId`), and that `/shipments` honours `batchId`.
- That `X-Rate-Limit-Reset` comes with each 429 on a big batch, and how long the order lookups take.
- Whether two shipments of one order in different batches (seconds apart) are both recorded: a waiting lookup for an order takes the newest batch's shipments.
