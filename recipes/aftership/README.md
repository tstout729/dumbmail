# AfterShip recipe

Checked against AfterShip's documentation on 2026-11-17; not yet run against a live account.

- **Signature.** `aftership-hmac-sha256` is "a base64-encoded HMAC generated using sha256 algorithm with webhook request body and webhook secret of your account". Source: [Webhook signature](https://www.aftership.com/docs/tracking/webhook/webhook-signature).
- **Body.** `{"event": "tracking_update", "event_id", "is_tracking_first_tag", "msg": {tracking}, "ts"}`. The tracking has `id`, `tracking_number`, `slug` (courier), `tag` and `subtag_message`, `order_number`, `aftership_tracking_url`, `latest_estimated_delivery`, `shipment_delivery_date`, `updated_at` and `customers[]` (`role`, `name`, `email`, `phone_number`). Source: [Webhook payload](https://www.aftership.com/docs/tracking/webhook/webhook-payload).
- **Statuses.** `InTransit` → Shipment In Transit, `OutForDelivery` → Out For Delivery, `Delivered` → Delivered, `Exception` and `AttemptFail` → Shipment Exception, once per shipment and status. `Pending`, `InfoReceived`, `AvailableForPickup` and `Expired` aren't recorded. Source: [Delivery statuses](https://www.aftership.com/docs/tracking/enum/delivery-statuses).
- **Resends.** `event_id` is the message id.
- **Who.** The first customer with an email whose `role` isn't `sender`. Shipments aren't consent: nobody is subscribed.

## Removed before anything is stored

Raw addresses, postal codes, coordinates, proof-of-delivery photos, who signed, notes, the customer list and the order-status link (it encodes the email).

## What a live check should look at first

- The `role` values AfterShip uses for the buyer (the documented example shows only `sender`).
