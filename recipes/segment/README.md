# Segment recipe

Checked against Segment's documentation on 2026-11-17; not yet run against a live workspace.

- **Signature.** With a Shared Secret set, the Webhooks destination signs each request in `X-Signature`: a hex HMAC-SHA1 of the body. With batching on, it signs only the first event of the batch, so batching must stay off. Source: [Webhooks destination](https://segment.com/docs/connections/destinations/catalog/webhooks/) (the classic destination; Webhooks (Actions) works the same for the shared secret).
- **Body.** One Segment call per request: `{"type", "userId", "anonymousId", "messageId", "timestamp", "traits"|"properties", "event", "context"}`. Sources: the destination's examples, [Identify](https://segment.com/docs/connections/spec/identify/), [Track](https://segment.com/docs/connections/spec/track/), [Common fields](https://segment.com/docs/connections/spec/common/).
- **Resends.** `messageId` is the message id and each event's `unique_id`.
- **Identify.** `traits.email` upserts the profile, matched first by the `userId` (as the external id `segment:<userId>`), then by email, and saves the `userId` as a key. So a new email for the same user changes that profile's address (its consent doesn't carry over to the new address) instead of putting the `userId` on a second profile. Names and phones aren't taken from calls.
- **Track.** Recorded only for names in `settings.events`, which start as Segment's ecommerce events (Order Completed, Order Refunded, Order Cancelled, Checkout Started, Product Added, Product Viewed, Signed Up) and must also be in the integration's metrics. `properties.revenue` (or `value`) is the event's value. Only the ecommerce spec's properties are copied onto the event (order, product and cart ids, totals, currency, coupons, products with their spec fields and the like; not free-text ones such as review bodies, share messages, search queries or recipients), plus any listed in `settings.copy_properties`. The person is the one an identify gave that `userId` (retried until it has arrived); an email inside a track call is never used to pick or create anyone. Source: [Ecommerce spec](https://segment.com/docs/connections/spec/ecommerce/v2/).
- **Trust.** Anyone with a source's write key can send calls, and browser sources' keys are public. Calls whose `context.library.name` is one of Segment's browser or mobile libraries (`analytics.js`, `analytics-next`, `analytics-ios`, `analytics-android`, `analytics-react-native`, `analytics-kotlin`, `analytics-swift`, `analytics-flutter`) are refused unless `settings.allow_client_sources` is `true`. That's a guard against mistakes, not proof (a caller can set the field), so connect only server-side sources you trust.
- **Consent.** Segment calls carry no marketing opt-in; nobody is subscribed.

## What a live check should look at first

- That the signature is over the exact body Segment sends.
- Whether track calls from the source carry `context.traits.email`.
