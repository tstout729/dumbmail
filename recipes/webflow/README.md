# Webflow forms recipe

Checked against Webflow's documentation on 2026-11-17; not yet run against a live site.

- **Signature.** `x-webflow-signature` is a hex HMAC-SHA256 of `"<x-webflow-timestamp>:<body>"`; the timestamp is in milliseconds and requests older than 5 minutes should be refused. Only webhooks created through the API (with an OAuth app's client secret, or a site token's per-webhook `secretKey`) carry these headers; dashboard-created ones don't, which is why the setup creates the webhook through the API. Webflow's Node example signs `JSON.stringify(req.body)` while its Python example signs the raw body; DumbMail checks the raw body. Source: [Working with webhooks](https://developers.webflow.com/data/docs/working-with-webhooks).
- **Signature window.** 5 minutes, as Webflow documents ("5 minutes (300,000 milliseconds)"). Source: [Working with webhooks](https://developers.webflow.com/data/docs/working-with-webhooks).
- **Body.** `{"triggerType": "form_submission", "payload": {"name", "siteId", "data", "schema", "submittedAt", "id", "formId", "formElementId", "localeId"}}`. `data` holds the submitted fields by name. Webflow's example payload is on its [All Events](https://developers.webflow.com/data/reference/all-events) page and in [Working with webhooks](https://developers.webflow.com/data/docs/working-with-webhooks) (there the email field is named `email`, and a real test submission's is `Email 2`, so set `email_field` to your form's name for it); the fixtures follow that shape with made-up values. Sources: [form_submission](https://developers.webflow.com/data/reference/webhooks/events/form-submission) and those pages.
- **Resends.** `payload.id` is the message id and the event's `unique_id`.
- **Who.** `data[settings.email_field]` (`Email` by default). Visitors are anonymous, so only the email is set; an existing profile's name never changes.
- **Consent.** A submission isn't an opt-in; nobody is subscribed. With `settings.consent_field` set to a checkbox's name, whether it was ticked is recorded as `marketing_consent` (`true`, `"true"`, `"on"`, `"yes"` or `"checked"` count as ticked).
- **Fields.** Only fields named in `settings.copy_fields` are copied onto the event.
- **What's stored.** Keep-only: the trigger type, the submission, form and site ids, the form name, the time, and the email, consent and `copy_fields` fields. Every other field (messages, names, phone numbers) is dropped before anything is stored. A field name containing `.`, `[`, `]`, `|`, `=` or `/` can't be mapped.

## What a live check should look at first

- That the raw body (not re-serialized JSON) is what Webflow signs.
- What value a ticked checkbox has in `data`.
