# Acuity Scheduling recipe

Checked against Acuity Scheduling's documentation on 2026-11-18; not yet run against a live account.

- **Webhooks.** Form-encoded POSTs (`application/x-www-form-urlencoded`) with `action` (`scheduled`, `rescheduled`, `canceled`, `changed`; webhooks made through the API use `appointment.scheduled` and so on), `id` (the appointment), `calendarID` and `appointmentTypeID`. Example body: `action=changed&id=13&calendarID=1&appointmentTypeID=13`. Set up in Integrations → Webhooks, or through `POST /api/v1/webhooks`. Sources: [Webhooks](https://developers.acuityscheduling.com/docs/webhooks), [Dynamic webhooks](https://developers.acuityscheduling.com/page/webhooks-webhooks-webhooks).
- **Signature.** `X-Acuity-Signature` is a base64 HMAC-SHA256 of the raw body, keyed with the account's API key; that's why the API key is also the integration's signing secret. Source: [Webhooks](https://developers.acuityscheduling.com/docs/webhooks).
- **Signature window.** Acuity's signature covers only the body and sends no timestamp, so there's no time window. A resend (Acuity retries for up to 24 hours) only makes DumbMail fetch the appointment again, and every event has a fixed id, so nothing is recorded twice. Source: [Webhooks](https://developers.acuityscheduling.com/docs/webhooks).
- **The appointment.** `GET /api/v1/appointments/{id}` with HTTP Basic (user ID and API key): `firstName`, `lastName`, `email`, `phone`, `datetime` (like `2013-06-17T10:15:00-0700`), `type`, `calendar`, `duration`, `timezone`, and more. "Canceled responses include a `noShow` field" (true when the admin marked a no-show), which is how the recipe tells a cancelled appointment; a `canceled: true` field is read too. Sources: [Get appointment](https://developers.acuityscheduling.com/reference/get-appointments-id), [List appointments](https://developers.acuityscheduling.com/reference/get-appointments).
- **Order.** The fetched appointment is what's recorded, not the webhook's action, so a late "scheduled" for an appointment cancelled since records the cancellation, never a booking. An appointment has no time of its last change, so events are dated when DumbMail received Acuity's answer.
- **Events.** Appointment Scheduled (once per appointment), Appointment Rescheduled (once per new time, when the signed action says rescheduled), Appointment Cancelled (once, with `no_show`).
- **Consent.** Acuity's appointment has no marketing opt-in field. Clients are held back from marketing.

## Removed before anything is stored

An allow-list keeps the webhook's four fields and the appointment's id, client name, email, phone, time, type, calendar, length, time zone and cancellation fields. Intake form answers (`forms`, `formsText`), notes, prices and payments, gift certificate codes, the confirmation page link and labels are dropped.

## What a live check should look at first

- That `GET /appointments/{id}` returns a cancelled appointment (with `noShow`) rather than 404.
- The `action` values webhooks created in the Integrations page actually send, and that the signature matches the raw body.
- Whether two webhooks for one appointment (rescheduled, then canceled within seconds) leave the right final event.
