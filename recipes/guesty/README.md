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
- **Guests: `GET /v1/guests-crud` with `columns=id firstName lastName fullName guestEmail guestPhone`.** Only the list endpoint returns `guestEmail` and `guestPhone`; "get guest by id" doesn't. Sources: [Fields for guest reports](https://open-api-docs.guesty.com/docs/fields-for-guest-reports) and [Get guests list](https://open-api-docs.guesty.com/reference/guestsopenapicontroller_getguestslist). The script asks for a page's guests with `filters={"id":{"@in":[…]}}`. The docs show `filters` as a JSON object with `@in` (on another field), but don't show filtering by `id`. **Check this with Test pull too.** If Guesty ignores the filter, guests the page doesn't include are matched by their Guesty guest id when DumbMail already knows them, and skipped otherwise.
- **Token.** OAuth2 client credentials at `https://open-api.guesty.com/oauth2/token`, scope `open-api`. Guesty allows only a few tokens a day, so DumbMail reuses one until shortly before it expires and asks for at most 4 a day. Source: [Authentication](https://open-api-docs.guesty.com/docs/authentication).

## Money

The current reservation API and the v2 webhooks carry no totals. Guesty moved pricing to the guest folio (`GET /guest-folio/overview`) and payments to `GET /payments`; see the 5 November 2026 changelog. `Reservation Booked` has a value only when a legacy webhook delivered the reservation's `money`. **After 1 March 2027 bookings are recorded without a total** until the recipe reads the folio API.

## Events

- `Reservation Booked`: once per reservation (`unique_id` `guesty:<id>:booked`). It carries the value when known, so a date change or a resend never counts revenue again. The booking confirmation flow starts from this event.
- `Reservation Confirmed`: one per version (`guesty:<id>:confirmed:<version time>`), dated by Guesty's own time. Flows keyed on `reservation_id` follow the newest. A late, older version never replaces a newer one, and A → B → A dates give three versions with the last one current.
- `Reservation Cancelled`: recorded when it is first seen on any path, dated at that moment. So it always stops reminders for every confirmation recorded before it, even though webhook and pull times come from different clocks.
- `Reservation Updated`: webhooks only, with what changed.
- `Inquiry Received`.

## Things with an end date

- Legacy webhooks (`reservation.new`, `reservation.updated`) and the legacy reservations API stop working on **1 March 2027**. The recipe works without them. Only booking totals depend on the legacy webhooks (see Money).
