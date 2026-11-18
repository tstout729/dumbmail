# Sender screening

Every customer on the hosted service sends email through one Amazon SES account and texts through one Telnyx account. A few senders of cold email, bought lists or phishing would drag everyone's delivery down, so DumbMail checks who signs up, the files they import, the domains they add, what they send and how people react to it. Most checks are rules plus one call to Claude (Anthropic's AI). Anything unclear goes to a person.

A self-hosted install screens nothing.

## Standing

Each account has a sending review status:

| Status | What it means | What it does |
| --- | --- | --- |
| `pending` | A new account, checked before its first real send. | Campaigns can't be sent or scheduled. Flows can be turned on; their emails and texts wait (tried again every 15 minutes) and are skipped with the reason `account not approved` after 3 days. Test emails go only to the team's own addresses; test texts wait. |
| `approved` | Checked and fine. Accounts that existed before screening started are approved. | Sends as normal. Domains, imports, content and engagement are still checked. |
| `review` | A person is checking the account. | Like `pending`. Campaigns in progress stop (their unsent messages are skipped with a reason). |
| `blocked` | The account can't send. | No marketing email or texts: campaigns stop (scheduled ones too), flow messages are skipped with the reason `account blocked`, tests are refused. Billing, data export and closing the account keep working. |

Push notifications, transactional email (it has its own restricted mode) and replies to people who texted in aren't affected.

When an account moves to `review` or `blocked`, DumbMail's operator gets an alert. When it moves to `approved` or `blocked`, the account's owner gets an email.

Customers see the status and the reasons in Settings → Sending and before they send. API: `GET /api/screening` (MCP `get_sending_review`).

## The first check

It runs when a pending account first tries to send for real (sends or schedules a campaign, turns on a flow, or a flow step comes due) and when the customer asks for a review. First the customer answers three questions, stored on the account: what the business does, how the people on the list signed up, and about how many emails a month they'll send. API: `POST /api/screening/request` with `{business, list_source, monthly_volume}` (MCP `request_sending_review`).

The check waits until the account has a verified sending domain, and runs as soon as one is verified.

Rules first:

- **Block**: the owner signed up with a throwaway email address; a sending or link domain impersonates a well-known brand (see Domains).
- **Review**: no website; the website doesn't answer, is parked or for sale, is a placeholder, or is on a free hosting address; the sign-up email's domain, the website and the sending domain are three unrelated names; a sending or link domain registered less than 30 days ago; an earlier domain or import check said `review`.

Then one Claude call. A `pass` approves the account, `review` holds it for a person, `block` blocks it. The model can never lighten the rules' verdict. Rules that block decide without a model call. If there's no API key, or the model fails or declines, a rules `pass` becomes `review`: a person decides, and the operator alert says so.

## Domains

Checked when a sending domain or link domain is added, before anything is set up with SES or the host.

- **Block**: the name contains a well-known brand it doesn't belong to (banks and payment companies, Apple, Microsoft, Google, Amazon, PayPal, Meta, Netflix, carriers and delivery companies, and others). A distinctive brand counts as a whole word (`netflix-billing.co`) or run together with a word like secure, login or verify (`paypalsecure.com`). Brand names that are also ordinary words (apple, chase, amazon, ups) count only next to such a word or in a lookalike spelling (`app1e.com`, `rnicrosoft.net`). An account whose own website is on the brand's domain may use it.
- **Review**: registered less than 30 days ago (looked up with RDAP; a failed lookup is not held against anyone), or a name that reads as random characters.

A blocked domain is refused with the reason. A pending account becomes `blocked`; an approved one goes to `review`. A `review` is recorded: a pending account's first check sees it, and an approved account keeps sending.

## Imports

Every CSV import of marketing contacts is checked before any row is written. An import of people as unsubscribed (a suppression file) isn't checked. The Klaviyo import and integration syncs aren't CSV imports and aren't checked; the first check sees how many contacts came from them.

What's measured: rows; the share with no valid address; shared inboxes (info@, sales@) and throwaway addresses; the 20 most common address domains; the share on domains with no mail server (up to 200 domains looked up, within 10 seconds; consumer domains like gmail.com count as having one); misspelled domains (gmial.com, hotmial.com, yaho.com...); consumer versus company domains; any consent or sign-up date column. Shares are judged once a file has at least 20 addresses.

- **Block**: the file name or a column name says where a bought or scraped list came from (purchased, bought, scraped, leads, lead list, prospects, apollo, zoominfo, seamless, lusha, rocketreach, hunter, snov, linkedin, sales navigator, data axle, infousa; whole words, any case); more than 15% shared inboxes; more than 10% of addresses on domains without a mail server.
- **Review**: columns typical of prospect lists (a job title, a company size and a LinkedIn or company address); more than 5% shared inboxes; more than 3% on domains without a mail server; more than 3% misspelled domains.

Then one Claude call with only the file name, the column names and those statistics. A `block` refuses the whole import (nothing is written); a pending account becomes `blocked`, an approved one goes to `review`. A `review` lets the import through, and an approved account goes to `review` before it can send again. If the model can't be reached, the rules decide.

## Content

Checked when a campaign is sent or scheduled, when a flow is turned on, and when a live flow gets new email or text content. What's looked at: each message's subject, preview text, visible text (at most 6,000 characters), links and the domains they go to; for texts, the body. Every version of an A/B test is checked.

Rules first:

- **Block**: words that ask people to sign in or confirm account details ("verify your account", "confirm your password", "unusual sign-in") with a link to a domain that isn't the account's.
- **Review**: a link whose text shows one domain and goes to another; a link shortener (bit.ly and others) in an account's first campaign; a subject starting with "Re:" or "Fwd:" on a campaign.

Then one Claude call against the [acceptable use rules](ACCEPTABLE_USE.md).

- `pass`: it sends.
- `review`: it's held. The campaign can't be sent (a flow can't be turned on, or the edit isn't saved); the account stays approved and the operator is alerted. Once a person passes it, sending the same content goes through.
- `block`: it's refused and the account goes to `review`.

The same content is judged once: relaunching or rescheduling it unchanged uses the earlier verdict and costs nothing.

If the model can't be reached for an established account (approved, at least 1,000 real emails over at least 14 days, no pause in the last 30 days), the rules decide and the send goes ahead, and the campaign's screening says it was checked by rules only. For any other account the content is held for a person.

Sending waits for the check (up to 45 seconds; the dashboard shows "Checking your email…").

## How people react

About hourly per account, from message data DumbMail already has:

- A campaign with at least 300 delivered emails, more than 24 hours after its first send, with unique opens (machine opens included) under 5%, unsubscribes over 2%, or spam complaints over the campaign limit (0.5%, at least 3 complaints).
- The account's last 7 days of campaign email delivered more than 24 hours ago, at least 1,000 emails, with unique opens under 8%.

Either one pauses the account's email (as high bounces do) with a reason naming the campaign and the rate, and puts the account in `review`. DumbMail always tracks opens on campaign email, so no campaign is left out of the open-rate checks.

## What goes to Anthropic

Requests go only to `https://api.anthropic.com/v1/messages` (model `claude-opus-5-5`, structured output, server-side fallback), at most 4 at once across the server and 30 per account per day (past that, a check is decided by a person). Each check sends only:

- **First check**: the account's name; its website's title, description and visible text (at most 4,000 characters); the three answers; the domain of the owner's sign-up address (never the address); the sending and link domains and their age; summaries of earlier import checks (file names, column names, statistics); counts of contacts from other sources; and the first campaign's or flow's rendered content, if any.
- **Import**: the file name, column names and the statistics above.
- **Content**: the account's name, website domain and own domains; the campaign or flow name; each message's subject, preview text, visible text and links. Personalization is filled with a sample person ("Alex Rivera").

Never sent: a contact's address, name or phone number, or any row of an import.

The customer's material is marked as data the model must not take instructions from, and every answer is checked against the expected form. The model, its verdict and token counts are recorded on the screening.

## For the operator

All with `Authorization: Bearer $ADMIN_TOKEN`.

- `GET /admin/screening`: accounts that are pending (with what they're waiting for), in review or blocked, or have held content, newest first, each with its screenings (reasons, signals, source, model) and the three answers.
- `POST /admin/screening/decide` with `{"account_id": "…", "decision": "approve" | "review" | "block", "note": "…"}`. Approving also lifts an email pause, so one call lets the account send again. The note is shown to the customer.
- `POST /admin/screening/pass` with `{"screening_id": "…"}`: let a held campaign or flow go. The owner is emailed to send it again.

Every decision is recorded as a screening with source `operator`.
