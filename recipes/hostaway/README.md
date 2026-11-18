# Hostaway recipe

Checked against Hostaway's API documentation (https://api.hostaway.com/documentation) on 2026-11-17; not yet run against a live account.

- **Why a pull, not webhooks.** Hostaway's unified webhooks cover "reservation created", "reservation updated" and "new message received", and authenticate with an optional login and password "on the authentication header", but the documentation doesn't show the body they send. Rather than guess it, the recipe reads reservations from the API every 15 minutes. Source: Webhook events.
- **Token.** OAuth 2.0 client credentials: `POST https://api.hostaway.com/v1/accessTokens` with `client_id` (the account ID), `client_secret` (the API key) and `scope=general`; the answer's `expires_in` is about six months. DumbMail reuses the token and asks for at most two a day. Source: Authentication.
- **Changed reservations.** `GET /v1/reservations` returns `{"status": "success", "result": [...]}`, ordered by `updatedOn DESC, id DESC` by default, paged with `afterId` (the id of the last reservation on the page; `offset` is deprecated). Each run reads newest first and stops at the newest change the previous run read; that change becomes the next cursor. Hostaway notes that reservations updated during paging can shift between pages; such a reservation is read again on the next run. Sources: Retrieve a reservations list, Cursor-based pagination, Sorting by updatedOn.
- **Times.** `updatedOn` and `reservationDate` look like `2026-11-17 09:00:00`, with no zone given; they're read as UTC.
- **Statuses.** `new`, `modified` (and the deprecated `confirmed`) are bookings; `cancelled`, `declined` and `expired` are cancellations; `inquiry…` statuses are inquiries; `ownerStay` is skipped; `pending`, `awaitingPayment`, `unconfirmed`, `awaitingGuestVerification` and `unknown` wait until they become bookings. Source: Reservation statuses.
- **Paid in full.** `paid_in_full` is true when `isPaid` is true or `remainingBalance` is 0 on a booking with a total. The suggested pre-arrival flow only fires for versions where it's true, so arrival details never go to a stay that isn't fully paid. Source: the Reservation object.
- **Money.** `totalPrice` in `currency`, recorded once per reservation as Reservation Total.
- **Guests.** `guestEmail`, `guestFirstName`, `guestLastName`, `phone`. Booking-site relay addresses (Airbnb and others) are suppressed from email automatically. Nobody is subscribed: a reservation carries no marketing opt-in.

## Door codes and access details

Reservations carry `doorCode`, `doorCodeVendor` and `doorCodeInstruction`; listings carry `wifiUsername` and `wifiPassword`. The script never puts them in events or profiles, and the redact list removes them (with guest notes, the guest's address, picture, payment method, rental agreement link, custom fields and damage-protection screening) from every stored page.

## What a live check should look at first

- That `updatedOn` is present on each reservation in the list (the documented example object omits it; the sorting section shows it), and which time zone it's in.
- That `afterId` keeps paging in the default order.
- Whether the unified webhook's body is documented anywhere else; if so, add webhooks for faster updates.
