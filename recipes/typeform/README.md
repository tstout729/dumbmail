# Typeform recipe

Checked against Typeform's documentation on 2026-11-17; not yet run against a live account.

- **Signature.** `Typeform-Signature: sha256=<base64>`, an HMAC-SHA256 of the raw body keyed with the webhook's secret (a value you choose and set on the webhook). Source: [Secure your webhooks](https://www.typeform.com/developers/webhooks/secure-your-webhooks/).
- **Body.** `{"event_id", "event_type": "form_response", "form_response": {"form_id", "token", "submitted_at", "definition", "answers": [...], "hidden"}}`. Each answer has a `type` (`text`, `email`, `choice`, `choices`, `boolean`, `number`, `date`, `url`, `file_url`, `payment`, …), a value under that key, and `field` (`id`, `type`, `ref`). Unanswered questions are left out. Source: [Example payload](https://www.typeform.com/developers/webhooks/example-payload/).
- **Resends.** `event_id` is the message id; the event's `unique_id` is the form id and response `token`, so a response is recorded once.
- **Who.** The first `email` answer (or the one whose ref is `settings.email_field`). `hidden` fields come from the form's link and aren't trusted for identity. Respondents are anonymous web visitors, so the recipe only sets the email: it never changes an existing profile's name.
- **Consent.** A response isn't an opt-in, so nobody is subscribed. With `settings.consent_field` set to a yes/no or legal question's ref, its answer is recorded as `marketing_consent` on the event. Subscribing someone from it needs a confirmation (double opt-in) DumbMail can't yet start from an integration.
- **Answers.** Choice, yes/no, number and date answers are copied onto the event by ref. Free-text answers aren't copied (people type anything into them) and are removed from the stored payload, with payment, file upload, signature and recording answers.

## What a live check should look at first

- That the signature matches a real delivery.
- How a "legal" question's answer arrives (`boolean`), so `consent_field` works for it.
