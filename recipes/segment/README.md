# Segment recipe

Checked against Segment's documentation on 2026-11-17; not yet run against a live workspace.

- **Signature.** With a Shared Secret set, the Webhooks destination signs each request in `X-Signature`: a hex HMAC-SHA1 of the body. With batching on, it signs only the first event of the batch, so batching must stay off. Source: [Webhooks destination](https://segment.com/docs/connections/destinations/catalog/webhooks/) (the classic destination; Webhooks (Actions) works the same for the shared secret).
- **Body.** One Segment call per request: `{"type", "userId", "anonymousId", "messageId", "timestamp", "traits"|"properties", "event", "context"}`. Sources: the destination's examples, [Identify](https://segment.com/docs/connections/spec/identify/), [Track](https://segment.com/docs/connections/spec/track/), [Common fields](https://segment.com/docs/connections/spec/common/).
- **Resends.** `messageId` is the message id and each event's `unique_id`.
- **Identify.** `traits.email` (with `firstName`/`lastName` or `name`, and `phone`) upserts the profile by email; the `userId` is saved as a key. A `userId` is never used to change someone's email.
- **Track.** Recorded only for names in `settings.events`, which start as Segment's ecommerce events (Order Completed, Order Refunded, Order Cancelled, Checkout Started, Product Added, Product Viewed, Signed Up) and must also be in the integration's metrics. `properties.revenue` (or `value`) is the event's value. The person is the email in `context.traits` or `properties`, else the `userId` an identify saved (retried until it has arrived). Source: [Ecommerce spec](https://segment.com/docs/connections/spec/ecommerce/v2/).
- **Trust.** Anyone with a source's write key can send calls, and browser sources' keys are public. Connect only server-side sources you trust.
- **Consent.** Segment calls carry no marketing opt-in; nobody is subscribed.

## What a live check should look at first

- That the signature is over the exact body Segment sends.
- Whether track calls from the source carry `context.traits.email`.
