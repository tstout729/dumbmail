# Hostaway recipe

Checked against Hostaway's API documentation (https://api.hostaway.com/documentation) on 2026-11-17; not yet run against a live account.

- **Why a pull, not webhooks.** Hostaway's unified webhooks cover "reservation created", "reservation updated" and "new message received", and authenticate with an optional login and password "on the authentication header", but the documentation doesn't show the body they send. Rather than guess it, the recipe reads reservations from the API every 15 minutes. Source: Webhook events.
- **Signature window.** None: this recipe doesn't take webhooks; it reads Hostaway's API with its own token.
- **Token.** OAuth 2.0 client credentials: `POST https://api.hostaway.com/v1/accessTokens` with `client_id` (the account ID), `client_secret` (the API key) and `scope=general`; the answer's `expires_in` is about six months. DumbMail reuses the token and asks for at most two a day. Source: Authentication.
- **Changed reservations.** `GET /v1/reservations` returns `{"status": "success", "result": [...]}`, ordered by `updatedOn DESC, id DESC` by default, paged with `afterId` (the id of the last reservation on the page; `offset` is deprecated). Each run reads newest first back to the boundary the previous run saved: its newest change's time and the ids read at that time, so a reservation changed in the same second but not yet read is still read. While a run pages it saves where it got to; if it stops at the page cap, the next run continues from there first. The boundary moves only when a run reaches it. Hostaway notes reservations updated during paging can shift between pages; such a reservation is read again on a later run. Sources: Retrieve a reservations list, Cursor-based pagination, Sorting by updatedOn.
- **Times.** `updatedOn` and `reservationDate` look like `2026-11-17 09:00:00`, with no zone given; they're read as UTC.
- **Statuses.** `new`, `modified` (and the deprecated `confirmed`) are bookings; `cancelled`, `declined` and `expired` are cancellations; `inquiry…` statuses are inquiries; `ownerStay` is skipped; `pending`, `awaitingPayment`, `unconfirmed`, `awaitingGuestVerification` and `unknown` are Reservation Unconfirmed. Source: Reservation statuses.
- **Paid in full.** `remainingBalance` is "the difference between reservation total price and amount already paid by guest" and is returned "only if includePayments=1", so the pull sends `includePayments=1`. `paid_in_full` is true only when every signal agrees: a booked status (`new`, `modified`, `confirmed`), `totalPrice` above zero, `remainingBalance` present and exactly zero, and `isPaid` not false. A missing balance, or `isPaid` disagreeing with the balance, means not paid. The suggested pre-arrival flow fires only for a version where it's true. Source: the Reservation object.
- **Going back.** A newer version that isn't paid replaces the paid one (the flow follows the newest version by `reservation_id`), and a version that's `pending`, `awaitingPayment` or otherwise not booked is recorded as Reservation Unconfirmed, which cancels a waiting reminder. So an older "paid" version can never send arrival details for a stay that's no longer paid.
- **Money.** `totalPrice` in `currency`, recorded once per reservation as Reservation Total.
- **Guests.** `guestEmail`, `guestFirstName`, `guestLastName`, `phone`. Booking-site relay addresses (Airbnb and others) are suppressed from email automatically. Nobody is subscribed: a reservation carries no marketing opt-in.

## Door codes and access details

Reservations carry `doorCode`, `doorCodeVendor` and `doorCodeInstruction`; listings carry `wifiUsername` and `wifiPassword`. The script never puts them in events or profiles, and the redact list removes them (with guest notes, the guest's address, picture, payment method, rental agreement link, guest portal link and its auth hash, custom fields and damage-protection screening) from every stored page.

## What a live check should look at first

- That `updatedOn` is present on each reservation in the list (the documented example object omits it; the sorting section shows it), and which time zone it's in.
- That `afterId` keeps paging in the default order.
- Whether the unified webhook's body is documented anywhere else; if so, add webhooks for faster updates.
