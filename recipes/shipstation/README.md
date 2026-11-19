# ShipStation recipe

Checked against ShipStation's documentation on 2026-11-18; not yet run against a live account.

- **Webhooks.** `POST /webhooks/subscribe` with `target_url` and `event` (`SHIP_NOTIFY`, `ITEM_SHIP_NOTIFY` and others), or in ShipStation's settings, where they're called On Orders Shipped and On Items Shipped; both fire when a label is created. The body is a Webhook object: `resource_url` ("Use this URL to get the resource that triggered the webhook", up to 200 characters, Basic auth) and `resource_type`. Example: `{"resource_url": "https://ssapi.shipstation.com/shipments?storeID=123456&batchId=12345678", "resource_type": "SHIP_NOTIFY"}`. Sources: [Subscribe to webhook](https://www.shipstation.com/docs/api/webhooks/subscribe/), [Webhook model](https://www.shipstation.com/docs/api/models/webhook/), [ShipStation webhooks (help center)](https://help.shipstation.com/hc/en-us/articles/360025856252-ShipStation-Webhooks).
- **Authentication.** ShipStation doesn't sign webhooks. The integration's URL is the secret, so there's no signature or window, and nothing in a webhook is trusted: the `resource_url` must be a ShipStation `/shipments` URL naming a `batchId`, and only its batch and store ids (digits) are used, to fetch the shipments with your API key.
- **Shipments.** `GET /shipments` (the batch's ids, `page`, `pageSize` up to 500; 50 here): `shipmentId`, `orderId`, `orderNumber`, `trackingNumber`, `carrierCode`, `serviceCode`, `shipDate`, `voided`, `isReturnLabel`, and `total`/`page`/`pages`. A shipment has no customer email. The first page asks for the batch's other pages (up to 40). Source: [List shipments](https://docs.shipstation.com/apis/shipstation-v1/openapi/shipments/list_shipments).
- **Orders.** Each shipped order is fetched (`GET /orders/{orderId}`) for its `customerEmail`, carrying that page's shipments for it. Each such lookup is its own piece of work (the fetch identity is the record and its context), so a second batch naming the same order never replaces a waiting or running lookup's shipments. Sources: [Get order](https://www.shipstation.com/docs/api/orders/get-order/).
- **Scope.** Only the stores in `store_ids` are recorded: a batch whose `resource_url` names another store isn't fetched, and every order's `advancedOptions.storeId` ("ID of store that is associated with the order") must be one of them. Source: [AdvancedOptions](https://www.shipstation.com/docs/api/models/advanced-options/).
- **Rate limit.** 40 requests a minute per key; a 429 carries `X-Rate-Limit-Reset` (seconds until the window resets). The recipe declares both (`per_minute: 40`, `reset_header`): DumbMail keeps to 40 a minute, and after a 429 every waiting lookup for these credentials waits for the reset. It looks up at most 2,000 records an hour. Source: [Requirements](https://www.shipstation.com/docs/api/requirements/).
- **Big batches.** One webhook reads at most 50 pages of 50 shipments (its lookups chain at most three deep); a bigger batch fails visibly in the delivery log instead of being cut short.
- **Events.** Order Shipped, once per shipment id, on the order's customer. Voided labels and return labels aren't shipments.
- **Consent.** ShipStation's order has no marketing opt-in field. Customers are held back from marketing; the suggested shipping email is a service message, one per person at a time (shipments labelled together for the same person send one email).

## Removed before anything is stored

An allow-list keeps the webhook's two fields, the shipments' ids, numbers, tracking, carrier, service, ship date and void/return flags, the paging counts, and the order's id, number, customer email and store id. Ship-to and bill-to addresses, items, labels, notes and gift messages are dropped.

## What a live check should look at first

- The query keys real `resource_url`s carry (`storeID` or `storeId`, `batchId`), and that `/shipments` honours `batchId`.
- That `X-Rate-Limit-Reset` comes with each 429 on a big batch, and how long the order lookups take.
- That `advancedOptions.storeId` comes back on orders, matching the store ids in webhook URLs.
