# Gorgias recipe

Checked against Gorgias's documentation on 2026-11-18; not yet run against a live account.

- **HTTP integration.** Settings → HTTP integration → Manage → Add HTTP integration: a URL, a method, headers, a request content type (`application/json`), a body template, and triggers (Ticket created, Ticket updated, and others). Template variables like `{{ticket.customer.email}}` can be used in the URL and body; the ticket context has `id`. Sources: [HTTP integrations](https://docs.gorgias.com/data-and-http-integrations/http-integrations), [TicketHttpIntegration](https://developers.gorgias.com/reference/the-tickethttpintegration-object).
- **Authentication.** Gorgias doesn't sign requests; it sends the headers you set ("Header-based") or uses OAuth2. The recipe checks a secret `X-Webhook-Token` header. There's no timestamp, so no window, and the body only names the ticket: what's recorded always comes from fetching it. Header-based requests that time out aren't retried by Gorgias. Source: [HTTP integrations](https://docs.gorgias.com/data-and-http-integrations/http-integrations).
- **The ticket.** `GET https://<account>.gorgias.com/api/tickets/{id}` (HTTP Basic with your email and API key): `id`, `status`, `channel`, `spam`, `subject`, `customer` (with `email`), `created_datetime`, `closed_datetime`, `trashed_datetime`, `messages`, and `satisfaction_survey` (`id`, `score` 1 to 5, `scored_datetime`, `body_text`). Sources: [Ticket object](https://developers.gorgias.com/reference/the-ticket-object), [SatisfactionSurvey object](https://developers.gorgias.com/reference/the-satisfactionsurvey-object).
- **Events.** Ticket Created (once per ticket), Ticket Closed (once per `closed_datetime`, so a ticket closed again after reopening counts again), Satisfaction Rated (once per survey, with `score`, once `scored_datetime` is set). Spam and trashed tickets are skipped.
- **Surveys.** Gorgias's rules have a "Satisfaction survey responded" trigger, but its HTTP integration triggers don't list one, so the rating is recorded the next time Gorgias sends Ticket updated for the ticket. Source: [HTTP integrations](https://docs.gorgias.com/data-and-http-integrations/http-integrations).
- **Consent.** A support ticket isn't consent; customers are held back from marketing, and their names aren't taken.

## Removed before anything is stored

An allow-list keeps the request's `ticket_id`, and from the ticket its id, subject, status, channel, spam flag, dates, the customer's email and the survey's id, score and time. Messages, the survey comment, customer names and notes, and the shop data Gorgias attaches to customers are dropped.

## What a live check should look at first

- That `{{ticket.id}}` renders in the JSON body as written in the setup steps.
- Whether answering a satisfaction survey fires Ticket updated (if not, ratings wait for the ticket's next change).
- The `status` values (`open`, `closed`) and `closed_datetime` on a closed ticket.
