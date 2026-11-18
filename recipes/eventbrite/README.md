# Eventbrite recipe

Checked against Eventbrite's documentation on 2026-11-18; not yet run against a live account.

- **Webhooks.** JSON POSTs with `api_url` ("Object whose creation or update triggered the webhook") and `config` (`action`, `endpoint_url`, `user_id`, `webhook_id`). Actions include order placed, updated and refunded, and attendee updated, checked in and checked out. Created in Account settings → Developer links → Webhooks. A failed delivery is resent every ten minutes, up to ten more times. Source: [Using webhooks](https://www.eventbrite.com/platform/docs/webhooks).
- **Authentication.** Eventbrite doesn't sign webhooks; its docs say to send them to a "private, unpublished" URL. The integration's URL is that secret, so there's no signature or window. Nothing in a webhook is trusted: the `api_url` must be on `https://www.eventbriteapi.com/v3/`, its ids are only used to fetch the order or attendee with your token, and the action only decides what to fetch. Source: [Using webhooks](https://www.eventbrite.com/platform/docs/webhooks).
- **Orders.** `GET /v3/orders/{id}/` (Bearer private token): `id`, `email`, `first_name`, `last_name`, `status` (`placed` in the example), `event_id`, `created`, and `costs.gross` (`currency`, `value` in minor units, `major_value` as text). Placed Order needs `status: placed` and a gross above zero, valued from `major_value` in the order's currency. A free order (gross 0.00) is Registered for Event. Source: [Finding order information](https://www.eventbrite.com/platform/docs/order-lookup).
- **Refunds.** Eventbrite doesn't document a "refunded" order status; its attendees (one per ticket) carry `refunded` and their own `costs.gross`. On `order.refunded` the recipe looks the order up (so the buyer is known) and fetches `GET /v3/orders/{id}/attendees/`; each attendee marked `refunded` with a gross above zero is one Refunded Order, once per attendee id, on the buyer found by the order. Only the first page of an order's attendees is read. Sources: [Obtaining attendee information](https://www.eventbrite.com/platform/docs/attendees), [Cancel and refund order](https://www.eventbrite.com/platform/docs/cancelling-and-refunding-orders).
- **Check-ins.** Attendee webhooks point at `/v3/events/{event_id}/attendees/{attendee_id}/`. The attendee is fetched, and Checked In is recorded once when it says `checked_in: true`, on the attendee's own email. Check-ins may arrive named `barcode.checked_in`; both names are read. Source: [Obtaining attendee information](https://www.eventbrite.com/platform/docs/attendees).
- **Consent.** Eventbrite's order and attendee have no marketing opt-in field. Buyers and attendees are held back from marketing; attendees' names aren't taken (the buyer types them).

## Removed before anything is stored

An allow-list keeps the webhook's `api_url` and action, and from Eventbrite's answers the ids, status, buyer's email and name, event id, dates, gross amount and currency, and the attendee's email, check-in and refund flags and ticket class. Barcodes (they are the tickets), answers to custom questions, addresses and fees are dropped.

## What a live check should look at first

- The order `status` values Eventbrite really returns for paid, pending (pay-offline) and refunded orders.
- That `refunded` is set on each refunded attendee, and only once the money has gone back.
- Whether check-in webhooks are named `attendee.checked_in` or `barcode.checked_in`, and the shape of their `api_url`.
- That paths with the trailing slash are what the API wants (Eventbrite's examples all have it).
