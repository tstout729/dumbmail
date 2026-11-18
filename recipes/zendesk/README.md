# Zendesk recipe

Checked against Zendesk's documentation on 2026-11-17; not yet run against a live account.

- **Signature.** `X-Zendesk-Webhook-Signature` is `base64(HMACSHA256(TIMESTAMP + BODY))` keyed with the webhook's signing secret, with the timestamp from `X-Zendesk-Webhook-Signature-Timestamp` and no separator. The docs don't state the timestamp's format; the recipe reads it as ISO 8601 and refuses it if it's more than 5 minutes off. Source: [Verifying webhook authenticity](https://developer.zendesk.com/documentation/webhooks/verifying/).
- **Signature window.** 5 minutes. Zendesk signs a timestamp but doesn't say how old one may be; DumbMail allows 5 minutes. Source: [Verifying webhook authenticity](https://developer.zendesk.com/documentation/webhooks/verifying/).
- **Body.** Zendesk triggers notify a webhook with a JSON body you write from placeholders; the setup steps give it. It uses `{{ticket.id}}`, `{{ticket.title}}` (the subject), `{{ticket.status}}`, `{{ticket.via}}`, `{{ticket.requester.email}}`, `{{ticket.requester.name}}`, `{{ticket.created_at_with_timestamp}}` and `{{ticket.updated_at_with_timestamp}}` (ISO 8601 like `2013-12-12T05:35Z`; the script adds the missing seconds), and `{{satisfaction.current_rating}}` for ratings (legacy CSAT, Suite Growth and up). Sources: [Creating webhooks](https://developer.zendesk.com/documentation/webhooks/creating-and-monitoring-webhooks/) and [Placeholders reference](https://support.zendesk.com/hc/en-us/articles/4408886858138-Zendesk-Support-placeholders-reference). Accounts created after 17 January 2023 with custom statuses may prefer `{{ticket.status_category}}`.
- **Events.** Ticket Created (once per ticket), Ticket Solved and Satisfaction Rated, dated by Zendesk's update time. Their event ids come only from the signed body: the ticket id, the status or rating, and the update time. Zendesk's times are to the minute, so a retry counts once, and a ticket solved twice within one minute (or rated the same twice) also counts once. The `x-zendesk-webhook-invocation-id` header would tell those apart, but Zendesk doesn't sign headers, so it isn't used. Source: [Anatomy of a webhook request](https://developer.zendesk.com/documentation/webhooks/anatomy-of-a-webhook-request/) and [Webhooks API: invocation attempts](https://developer.zendesk.com/api-reference/webhooks/webhooks-api/webhooks/).
- **What's kept.** The subject and status only. The body never includes comments, and `comment`, `description` and `latest_comment` are removed from anything stored if someone adds them.
- **Names.** Anyone can open a ticket under any name, so the requester's name never changes a profile; only the email is used.
- **Consent.** A support ticket isn't consent: nobody is subscribed and requesters this adds are held back from marketing.
- **Gorgias.** Not built: its HTTP integrations aren't signed and send whatever body you configure, so it would be the same body-by-template approach with only a shared header; left for a later pass.

## What a live check should look at first

- That Zendesk escapes placeholder values for JSON (a subject with a quote mark would otherwise break the body; the docs don't say).
- The signature timestamp's real format.
