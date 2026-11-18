# Guesty recipe

What the script assumes, and the Guesty page each assumption comes from (read 2026-11-17).

## Webhooks

- **Use the v2 reservation events.** `reservation.created.v2` replaces `reservation.new`, and `reservation.updated.v2` replaces `reservation.updated` and `reservation_update_shortlist`. Guesty retires the legacy events on **1 March 2027**. Source: [Reservation webhooks migration guide](https://open-api-docs.guesty.com/docs/reservation-webhooks-migration-guide) and the [5 November 2026 changelog](https://open-api-docs.guesty.com/changelog/2026-11-05).
- **v2 payload.** It has `data` (`reservationId`, `confirmationCode`, `guestId`, `status`, `checkIn`, `checkOut`, `checkInDateLocalized`, `checkOutDateLocalized`, `guestsCount`, `platform`, `source`, `createdAt`, `confirmedAt`, `unitId`, …) and `meta` (`eventId`, `publishedAt`, and on updates `subType` and `changedFields`). It carries no guest email and no money. Source: the webhooks migration guide.
- **Order and versions.** Guesty says to "use `publishedAt` to determine the chronological order", so `meta.publishedAt` is the version time of a v2 event. Legacy events use the reservation's `lastUpdatedAt`. Source: the webhooks migration guide.
- **Resends.** Webhooks are delivered by Svix. The recipe ignores a second delivery with the same `svix-id` header. Guesty describes `meta.eventId` as the unique message id. Sources: [Webhooks](https://open-api-docs.guesty.com/docs/webhooks) and the webhooks migration guide.
- **Signature.** Svix (`svix-id`, `svix-timestamp`, `svix-signature`) with the `whsec_` secret from `GET /v1/webhooks-v2/secret`. Source: [Webhooks](https://open-api-docs.guesty.com/docs/webhooks).

## Pulls

- **Reservations: `GET /v1/reservations-v3/search`.** This replaces the legacy `GET /reservations`, which Guesty removes on 1 March 2027. Results are in `results`, paging is `skip`/`limit` (at most 100) with `pagination.hasMore`, and the id is `reservationId`. `stay[]` holds `listingId`, `guestsCount` and the localized dates. The response has no money fields. Sources: [Search reservations](https://open-api-docs.guesty.com/reference/reservationsopenapicontroller_searchreservations) and the [Reservation migration guide](https://open-api-docs.guesty.com/docs/reservation-migration).
- **Changed reservations: `filter[lastUpdatedAt][gte]` with `sort=lastUpdatedAt`, and each result's `lastUpdatedAt` as the next cursor.** The reservation migration guide says this ("Each result exposes `lastUpdatedAt` to use as your next cursor"). The API reference page doesn't list this filter or sort value yet, and Guesty answers an unsupported filter with HTTP 400. **Check it with Test pull before turning the pull on.** The script moves its cursor forward on every page, so a run that stops at its page limit picks up where it left off.
- **Guests: `GET /v1/guests-crud` with `columns=id firstName lastName fullName guestEmail guestPhone` and `filters={"_id":{"@in":[…]}}`.** Only the list endpoint returns guests' email and phone. Sources: [Fields for guest reports](https://open-api-docs.guesty.com/docs/fields-for-guest-reports) and [Get guests list](https://open-api-docs.guesty.com/reference/guestsopenapicontroller_getguestslist). Checked read-only against a real Guesty account on 17 November 2026: a filter on `id` matches nothing; the filter on `_id` works; and the answer names the fields `_id`, `email` and `phone` (not `id`, `guestEmail`, `guestPhone`), with the phone as digits including the country code but no `+` (the script adds it). Any of them can be missing (some channels give no email).
- **Token.** OAuth2 client credentials at `https://open-api.guesty.com/oauth2/token`, scope `open-api`. Guesty allows only a few tokens a day, so DumbMail reuses one until shortly before it expires and asks for at most 4 a day. Source: [Authentication](https://open-api-docs.guesty.com/docs/authentication).

## Money

Pre-arrival flows send only for bookings Guesty reports as **paid in full**: `paid_in_full` is true when the reservation's `money.balanceDue` is 0 and its total (`money.totalPaid` + `money.balanceDue`, or `money.hostPayout`) is above 0, false when something is still due, and missing (unknown) when a message carries no money. Unknown never sends. It's recorded on the `reservation` object and on the Reservation Booked and Reservation Confirmed events, and a change in it makes a new Reservation Confirmed version. Payment is written with the rest of the reservation, by Guesty's own time: an older message never overrides a newer state, and when a newer message without money arrived first, the payment status stays as it was (unknown sends nothing) until Guesty's next message that carries money.


The current reservation API and the v2 webhooks carry no totals. Guesty moved pricing to the guest folio (`GET /guest-folio/overview`) and payments to `GET /payments`; see the 5 November 2026 changelog. A booking's value is recorded as `Reservation Total` only when a legacy webhook delivers the reservation's `money`, once per reservation, whether it arrives before or after the messages without a total. **After 1 March 2027 bookings are recorded without a total** until the recipe reads the folio API.

## Events

- `Reservation Booked`: once per reservation (`unique_id` `guesty:<id>:booked`), from whichever message arrives first. The booking confirmation flow starts from this event.
- `Reservation Total`: the booking's value, once per reservation (`guesty:<id>:total`), so revenue counts once whatever order messages arrive in.
- `Reservation Confirmed`: one per version (`guesty:<id>:confirmed:<version time>`), dated by Guesty's own time (`meta.publishedAt` for v2 webhooks, `lastUpdatedAt` for pulls and legacy webhooks). A webhook makes a new version only when its dates, listing, status or guest changed (v2 `meta.changedFields`, or the legacy `reservationBefore`); a notes-only change makes none. Flows keyed on `reservation_id` follow the newest. A late, older version never replaces a newer one, and A → B → A dates give three versions with the last one current. The pre-arrival flow never sends the same booking's reminder twice for the same date.
- `Reservation Cancelled`: dated by Guesty's own time, like versions. So a cancellation that arrives late, after the booking was reinstated, is older than the reinstatement and doesn't stop its reminders.
- A webhook with no Guesty time at all can't be put in order, so it writes no events; the next pull (every 15 minutes) records the change with Guesty's time. A v2 webhook for a guest DumbMail doesn't know yet is retried (1, 5, 30 and 120 minutes) until the pull has added them.
- `Reservation Updated`: webhooks only, with what changed.
- `Inquiry Received`.

## Things with an end date

- Legacy webhooks (`reservation.new`, `reservation.updated`) and the legacy reservations API stop working on **1 March 2027**. The recipe works without them. Only booking totals depend on the legacy webhooks (see Money).

## Door codes and access notes

Live reservations carry door and key codes in `notes` (`notes.keyCode`, and in `notes.guest`) and contact details in `channelMetadata.guestContact`. The script never puts them in events or profiles, and the recipe's `redact` list (`**.notes`, `**.keyCode`, `**.doorCode`, `**.lockCode`, `**.accessCode`, `**.accessCodes`, `**.checkInInstructions`, `**.guestContact`, `**.wifiPassword`) removes them, at any depth, from webhook payloads (the reservation and its before-state) and pull pages before anything is stored or shown.

## Paging

Each page starts at the newest change already read (`filter[lastUpdatedAt][gte]`) and skips the reservations at exactly that time it already read (`skip`), both saved in the integration's state. So reservations that changed at the same moment are never read twice, even when a run stops at its page limit and the next one picks up.
