# Jotform recipe

Checked against Jotform's documentation on 2026-11-18; not yet run against a live account.

- **Webhooks.** Added in the Form Builder (Settings → Integrations → Webhooks) or with `POST /form/{id}/webhooks`. Jotform posts `multipart/form-data` with `formID`, `submissionID` and `rawRequest` (the answers as JSON) among other fields. DumbMail reads a multipart body as an ordinary form of its text fields, and the recipe keeps only `formID` and `submissionID`. Sources: [API docs](https://api.jotform.com/docs/), [Creating your first webhook](https://www.jotform.com/help/245-Creating-your-first-Webhook-with-JotForm), Jotform support answers [1211524](https://www.jotform.com/answers/1211524-receiving-webhooks-from-jotform) and [321875](https://www.jotform.com/answers/321875-webhook-access-to-the-submissionid-value).
- **Authentication.** Jotform doesn't sign webhooks. The integration's URL is the secret, so there's no signature or window, and nothing in a webhook is trusted: its `submissionID` is only used to fetch the submission with your API key, and only that answer is recorded.
- **The submission.** `GET /submission/{id}` with the `APIKEY` header: `content.id`, `content.form_id`, `content.created_at`, `content.status` and `content.answers`, keyed by question ID, each with `text`, `type`, `answer` and sometimes `prettyFormat` (a name's answer is an object; its `prettyFormat` is the text). Base URLs: `api.jotform.com`, `eu-api.jotform.com`, `hipaa-api.jotform.com`. Source: [API docs](https://api.jotform.com/docs/).
- **Mapping.** `form_id` is required: nothing is recorded until it's set, and every fetched submission must be from that form. `email_field` is the email question's ID (from `GET /form/{id}/questions`, with the `APIKEY` header) and must be an email question; `consent_field` and `consent_value` name a consent question and the exact answer that means yes (a checkbox's answer may be a list of ticked options); `copy_fields` lists the IDs of answers to copy onto the event. Only questions of these types can be mapped: full name, email, phone, short text, number, date, dropdown, radio buttons and checkboxes. Addresses, payments, signatures, file uploads and every other type are never kept, even if listed; Jotform's submission doesn't say whether a question is marked sensitive, so the type is the check.
- **Events.** Form Submitted, once per submission (`jotform:<form>:<submission>`). A form submission is never an opt-in (anyone can type an address into a form), so nobody is subscribed and nobody is renamed; the mapped consent answer is recorded as `marketing_consent` on the event.

## Removed before anything is stored

An allow-list keeps the webhook's `formID` and `submissionID`, and from the submission its id, form id, and the `type` and `answer` of the mapped questions of safe types only (a name's first and last parts; never Jotform's `prettyFormat` text). The framework's allow-list checks each answer's type before it's stored. Every other answer, the submitter's IP and `rawRequest` are dropped.

## What a live check should look at first

- That Jotform's webhook really arrives as `multipart/form-data` with `submissionID` as a text field.
- That `GET /submission/{id}` answers right after the webhook (not 404 for a moment).
- The `answer` shape of checkbox, dropdown and name questions, so `consent_value` and `copy_fields` match.
