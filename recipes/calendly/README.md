# Calendly recipe

What the script assumes, and the Calendly page each assumption comes from.

- **Signature.** The `Calendly-Webhook-Signature` header is `t=<unix>,v1=<hex HMAC-SHA256 of "t.body">`, signed with the subscription's `signing_key`. Source: [Webhook signatures](https://developer.calendly.com/api-docs/overview/webhooks/webhook-signatures).
- **Events.** `invitee.created` and `invitee.canceled`. Their `payload` is the Invitee: `uri`, `email`, `name`, `scheduled_event` (`start_time`, `end_time`, `name`), `cancel_url`, `reschedule_url`, `rescheduled`, and `cancellation`. Sources: [Webhook subscriptions](https://developer.calendly.com/receive-data-from-scheduled-events-in-real-time-with-webhook-subscriptions) and the [API spec](https://developer.calendly.com/openapi/calendly-api.yaml).
- **Keys.** The invitee `uri` identifies a booking. A reschedule cancels the old invitee and creates a new one, so reminders follow the new time.
- **No message id.** Calendly's payload has no delivery id. Resends are recognized by each event's `unique_id`: the invitee `uri` plus created or canceled.
