# Eventbrite recipe

Checked against Eventbrite's documentation on 2026-11-18; not yet run against a live account.

- **Webhooks.** JSON POSTs with `api_url` ("Object whose creation or update triggered the webhook") and `config` (`action`, `endpoint_url`, `user_id`, `webhook_id`). Actions include order placed, updated and refunded, and attendee updated, checked in and checked out. Created in Account settings → Developer links → Webhooks. A failed delivery is resent every ten minutes, up to ten more times. Source: [Using webhooks](https://www.eventbrite.com/platform/docs/webhooks).
- **Authentication.** Eventbrite doesn't sign webhooks; its docs say to send them to a "private, unpublished" URL. The integration's URL is that secret, so there's no signature or window. Nothing in a webhook is trusted: the `api_url` must be on `https://www.eventbriteapi.com/v3/`, its ids are only used to fetch the order or attendee with your token, and the action only decides what to fetch. Source: [Using webhooks](https://www.eventbrite.com/platform/docs/webhooks).
- **Scope.** Only the events listed in `event_ids` are recorded: the order's and attendee's documented `event_id` must be one of them. Eventbrite's event object doesn't document its organization, so the scope is the events.
- **Orders.** `GET /v3/orders/{id}/?expand=refund_requests` (Bearer private token): `id`, `email`, `first_name`, `last_name`, `status` (`placed` in the example), `event_id`, `created`, `costs.gross` and `costs.payment_fee` ("Payment processor fee as portion of Order gross amount"). Currency objects carry `value`, "the number of units of the minor unit of the currency"; amounts are that value at the currency's ISO 4217 precision (JPY none, KWD three), never `major_value` rounded. Placed Order needs `status: placed`, a gross above zero and a payment processor fee (paid through a payment processor). Any other placed order is Registered for Event, with no value. Sources: [Order](https://www.eventbrite.com/platform/api#/reference/order), [Currency](https://www.eventbrite.com/platform/api#/introduction/basic-types/currency), [Finding order information](https://www.eventbrite.com/platform/docs/order-lookup).
- **Refunds.** The order's `refund_requests` expansion holds refund items with `status` (`pending`, `processed` or `error`), `processed_date`, `amount_processed` ("absent if the refund has not been processed"), `item_type` and `order_id`. Each item with status `processed`, a processed date and a processed amount above zero, for this order, is one Refunded Order for that amount. A refund item has no id of its own, so its order, kind, processing time and amount identify it. Source: [Order](https://www.eventbrite.com/platform/api#/reference/order).
- **Check-ins.** Attendee webhooks point at `/v3/events/{event_id}/attendees/{attendee_id}/`. The attendee is fetched, and Checked In is recorded once when it says `checked_in: true`, on the attendee's own email. Check-ins may arrive named `barcode.checked_in`; both names are read. Source: [Obtaining attendee information](https://www.eventbrite.com/platform/docs/attendees).
- **Consent.** Eventbrite documents a `contact_list_preferences` expansion (opt-in preferences for the order's email); this recipe doesn't read it, so nobody is subscribed. Buyers and attendees are held back from marketing; attendees' names aren't taken (the buyer types them).

## Removed before anything is stored

An allow-list keeps the webhook's `api_url` and action, and from Eventbrite's answers the ids, status, buyer's email and name, event id, dates, the gross and payment fee values and currencies, the refund items' status, dates, kind and processed amounts (not the buyer's refund message), and the attendee's email, check-in flag and ticket class. Barcodes (they are the tickets), answers to custom questions, addresses and fees are dropped.

## What a live check should look at first

- The order `status` values Eventbrite really returns for paid, pending (pay-offline) and refunded orders.
- That a placed, paid order shows a `payment_fee` above zero, and an offline or invoiced one doesn't.
- The key the `refund_requests` expansion comes back under (`refund_requests` or `refund_request`; both are read), and that items reach `processed` once the money has gone back.
- Whether check-in webhooks are named `attendee.checked_in` or `barcode.checked_in`, and the shape of their `api_url`.
- That paths with the trailing slash are what the API wants (Eventbrite's examples all have it).
