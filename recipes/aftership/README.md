# AfterShip recipe

Checked against AfterShip's documentation on 2026-11-17; not yet run against a live account.

- **Signature.** `aftership-hmac-sha256` is "a base64-encoded HMAC generated using sha256 algorithm with webhook request body and webhook secret of your account". Source: [Webhook signature](https://www.aftership.com/docs/tracking/webhook/webhook-signature).
- **Body.** `{"event": "tracking_update", "event_id", "is_tracking_first_tag", "msg": {tracking}, "ts"}`. The tracking has `id`, `tracking_number`, `slug` (courier), `tag` and `subtag_message`, `order_number`, `aftership_tracking_url`, `latest_estimated_delivery`, `shipment_delivery_date`, `updated_at` and `customers[]` (`role`, `name`, `email`, `phone_number`). Source: [Webhook payload](https://www.aftership.com/docs/tracking/webhook/webhook-payload).
- **Statuses.** `InTransit` → Shipment In Transit, `OutForDelivery` → Out For Delivery, `Delivered` → Delivered, `Exception` and `AttemptFail` → Shipment Exception, once per shipment and status. `Pending`, `InfoReceived`, `AvailableForPickup` and `Expired` aren't recorded. Source: [Delivery statuses](https://www.aftership.com/docs/tracking/enum/delivery-statuses).
- **Resends.** `event_id` is the message id.
- **Who.** The customer whose `role` is `settings.recipient_role`: `buyer` by default, or `receiver`. AfterShip's help center documents those two roles (lowercase, case-sensitive) for customer-role notifications; the API model shows only `sender` as an example. A shipment with no customer in that role records nothing. Shipments aren't consent: nobody is subscribed. Sources: [Customer role-based shipment notifications](https://tracking-helpcenter.aftership.com/en/article/customer-role-based-shipment-notifications-yrs9eq/) and the [Tracking model](https://www.aftership.com/docs/tracking/2026-01/model/tracking).
- **Order.** Each event is dated by AfterShip's `updated_at` and carries the shipment `id` as `shipment_id`. The suggested "arrives today" flow is keyed on it and stands down when Delivered or Shipment Exception for that shipment is dated at or after the out-for-delivery update, so a late, older "out for delivery" never sends it.

## Removed before anything is stored

Raw addresses, postal codes, coordinates, proof-of-delivery photos, who signed, notes, the legacy email and SMS lists and the order-status link (it encodes the email). Customers' names are removed too; their email and phone stay in the stored payload, because the script runs on it.

## What a live check should look at first

- The `role` values real shipments carry (`buyer` or `receiver`), and whether `updated_at` moves forward with each status.
