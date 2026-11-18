# Webflow forms recipe

Checked against Webflow's documentation on 2026-11-17; not yet run against a live site.

- **Signature.** `x-webflow-signature` is a hex HMAC-SHA256 of `"<x-webflow-timestamp>:<body>"`; the timestamp is in milliseconds and requests older than 5 minutes should be refused. Only webhooks created through the API (with an OAuth app's client secret, or a site token's per-webhook `secretKey`) carry these headers; dashboard-created ones don't, which is why the setup creates the webhook through the API. Webflow's Node example signs `JSON.stringify(req.body)` while its Python example signs the raw body; DumbMail checks the raw body. Source: [Working with webhooks](https://developers.webflow.com/data/docs/working-with-webhooks).
- **Body.** `{"triggerType": "form_submission", "payload": {"name", "siteId", "data", "schema", "submittedAt", "id", "formId", "formElementId", "localeId"}}`. `data` holds the submitted fields by name. The page documents the fields but shows no full example, so the fixtures use made-up values in that shape. Source: [form_submission](https://developers.webflow.com/data/reference/webhooks/events/form-submission).
- **Resends.** `payload.id` is the message id and the event's `unique_id`.
- **Who.** `data[settings.email_field]`, or the first field named like "email" that holds an address. Visitors are anonymous, so only the email is set; an existing profile's name never changes.
- **Consent.** A submission isn't an opt-in; nobody is subscribed. With `settings.consent_field` set to a checkbox's name, whether it was ticked is recorded as `marketing_consent` (`true`, `"true"`, `"on"`, `"yes"` or `"checked"` count as ticked).
- **Fields.** Only fields named in `settings.copy_fields` are copied onto the event, and the whole `data` object is removed from the stored payload.

## What a live check should look at first

- That the raw body (not re-serialized JSON) is what Webflow signs.
- What value a ticked checkbox has in `data`.
