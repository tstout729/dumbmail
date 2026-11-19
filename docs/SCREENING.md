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
| `blocked` | The account can't send. | No marketing email, texts or push: campaigns stop (scheduled ones too), flow messages are skipped with the reason `account blocked`, tests are refused. Billing, data export and closing the account keep working. |

Also while an account isn't approved:

- Texts the account writes itself in the inbox (replies typed in the inbox, keyword replies, automatic replies) don't go out; the reply box says why. DumbMail's own fixed compliance texts (the HELP answer, the sign-up question and the sign-up confirmation) do.
- Flow notifications go only to the account's members; while it's blocked, to nobody.
- An account in review because of an engagement pause: its flow email waits (up to 3 days, like any flow message) instead of being skipped by the pause; campaigns stop as the pause stops them.

WhatsApp follows the standing exactly like texts: campaigns, flow steps, inbox and keyword replies, tests (which go only to a team member's verified number) and its own final claim. Its opt-out answers always go; its sign-up confirmation goes before approval only in DumbMail's own wording. The text of a WhatsApp template, filled with the message's values, is screened like any message.

Push isn't held for a pending or in-review account; a blocked account sends no push. Transactional email has its own restricted mode; a store's hub sign-in codes (a person asked for one) go from pending and in-review accounts, and not from a blocked one.

The standing is read twice: early, from the account the send started with, and again in the statement that makes the final claim before the provider is called (an email's last check, a text's claim, a push's claim; a test email checks again that it goes to a current member). A message past that claim has been handed to the provider and can't be recalled.

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

- **Block**: the name contains a well-known brand it doesn't belong to (banks and payment companies, Apple, Microsoft, Google, Amazon, PayPal, Meta, Netflix, carriers and delivery companies, and others). A distinctive brand counts as a whole word (`netflix-billing.co`) or run together with a word like secure, login or verify (`paypalsecure.com`). Brand names that are also ordinary words (apple, chase, amazon, ups) count only next to such a word or in a lookalike spelling (`app1e.com`, `rnicrosoft.net`). Only proof exempts an account: a verified sending domain on the brand's own domain, or an operator marking the account as the brand's (`affiliated_brands` below). The website an account typed proves nothing.

Names are compared by their registrable domain, using the public suffix list (so `shop.github.io` and `evil.github.io` are different sites).
- **Review**: registered less than 30 days ago (looked up with RDAP; a failed lookup is not held against anyone), or a name that reads as random characters.

A blocked domain is refused with the reason. A pending account becomes `blocked`; an approved one goes to `review`. A `review` is recorded: a pending account's first check sees it, and an approved account keeps sending.

## Imports

Every CSV import of marketing contacts is checked before any row is written. An import of people as unsubscribed (a suppression file) isn't checked. The Klaviyo import and integration syncs aren't CSV imports and aren't checked; the first check sees how many contacts came from them.

What's measured: rows; the share with no valid address; shared inboxes (info@, sales@) and throwaway addresses; the 20 most common address domains; the share on domains with no mail server (up to 200 domains looked up, within 10 seconds; consumer domains like gmail.com count as having one); misspelled domains (gmial.com, hotmial.com, yaho.com...); consumer versus company domains; any consent or sign-up date column. Shares are judged once a file has at least 20 addresses.

A mail server lookup that fails or doesn't answer in time is unknown, never "no mail server". The no-mail-server share is over the addresses whose domain was actually checked, the reason says how much of the file that was, and a block on it needs at least 80% of the file's addresses checked.

A file that imports people as unsubscribed is a suppression file only if it subscribes nobody on any channel: with `sms_consent: subscribed` it's an ordinary import (permission confirmed, and checked).

- **Block**: the file name or a column name says where a bought or scraped list came from (purchased, bought, scraped, leads, lead list, prospects, apollo, zoominfo, seamless, lusha, rocketreach, hunter, snov, linkedin, sales navigator, data axle, infousa; whole words, any case); more than 15% shared inboxes; more than 10% of addresses on domains without a mail server.
- **Review**: columns typical of prospect lists (a job title, a company size and a LinkedIn or company address); more than 5% shared inboxes; more than 3% on domains without a mail server; more than 3% misspelled domains.

Then one Claude call with only the file name, the column names and those statistics. A `block` refuses the whole import (nothing is written); a pending account becomes `blocked`, an approved one goes to `review`. A `review` lets the import through, and an approved account goes to `review` before it can send again. If the model can't be reached, the rules decide.

## Content

Checked when a campaign is sent or scheduled, when a flow is turned on, when a live flow gets new email or text content, and when a saved block that a live flow uses changes. What's judged is the content exactly as committed, every way it can go out:

- every A/B version, and every language version;
- every block, whatever its display rules, and both sides of every template condition (the text as written, not one sample person's view);
- the saved blocks it uses, and review-request flow steps;
- for each message: subject, preview text, text (at most 20,000 characters), links and where they go; for texts, the body.

More than a check can cover (over 60 message versions, over 20,000 characters or 80 links in one, or more than 200 KB in all) isn't cut short: it's held for a person.

Rules first:

- **Block**: words that ask people to sign in or confirm account details ("verify your account", "confirm your password", "unusual sign-in") with a link to a domain that isn't the account's.
- **Review**: a link whose text shows one domain and goes to another; a link shortener (bit.ly and others) in an account's first campaign; a subject starting with "Re:" or "Fwd:" on a campaign.

Then one Claude call against the [acceptable use rules](ACCEPTABLE_USE.md).

- `pass`: it sends.
- `review`: it's held. The campaign can't be sent (a flow can't be turned on, or the edit isn't saved); the account stays approved and the operator is alerted. Once a person passes it, sending the same content goes through.
- `block`: it's refused and the account goes to `review`.

Each message version gets its own fingerprint, and sending checks what it's about to send against them: a campaign when its batch runs (it may have been edited after it was scheduled), a flow message as it goes out (a saved block or a translation may have changed it, or it was edited as a draft). Content that changed since its check waits while it's checked on the background lane (Claude is never called on the sending lane); held content keeps waiting (a flow message for up to 3 days), refused content isn't sent. A campaign whose content gets no verdict within 30 minutes of being due is held for a person.

The same content is judged once: relaunching or rescheduling it unchanged uses the earlier verdict and costs nothing. A pass the rules gave alone while Claude couldn't be reached is judged again the next time.

Content that was already live when screening started is recorded as passed (source `existing`), once, when the server starts after the upgrade, with no check: approved accounts' live flows (their steps, with the saved blocks and language versions they use) and their scheduled or sending campaigns. Nothing that was running waits or is held; any later edit is checked as usual.

**No API key set** (no `ANTHROPIC_API_KEY` on the server at all) isn't an outage: an approved account's content is judged by the rules alone and goes out if they pass (a rules review or block still holds it). A new account's first check still goes to a person. The operator page says when no key is set.

If a key is set but the model can't be reached for an established account (approved, at least 1,000 real emails over at least 14 days, no pause in the last 30 days), the rules decide and the send goes ahead, and the campaign's screening says it was checked by rules only. For any other account the content is held for a person.

Sending waits for the check: everything (rendering, the checks, the notices) within 45 seconds; the dashboard shows "Checking your email…".

## How people react

About hourly per account, from message data DumbMail already has:

- A campaign with at least 300 delivered emails, more than 24 hours after its first send, with unique opens (machine opens included) under 5%, unsubscribes over 2%, or spam complaints over the campaign limit (0.5%, at least 3 complaints).
- The account's last 7 days of campaign email delivered more than 24 hours ago, at least 1,000 emails, with unique opens under 8%.

Times are each email's own send time, and "delivered" means a delivery event arrived (the provider accepting an email isn't enough).

Either one pauses the account's email (as high bounces do) with a reason naming the campaign and the rate, and puts the account in `review`. DumbMail always tracks opens on campaign email, so no campaign is left out of the open-rate checks.

## What goes to Anthropic

Requests go only to `https://api.anthropic.com/v1/messages` (model `claude-opus-5-5`, structured output, server-side fallback). At most 4 run at once; that's counted in the server process, and the hosted service runs one. At most 30 per account per day, counted by a fixed Pacific day (changing the account's time zone doesn't reset it); past that, a check is decided by a person.

Each check sends only an allow-listed set of fields. In free text (website text, the three answers, message text, file and column names) anything that looks like an email address becomes `[email]` and anything that looks like a phone number `[phone]`. Links go as domain and path only: no query string or fragment, and any path part that looks like an id, a token, an address or a number replaced.

- **First check**: the account's name; its website's title, description and visible text (at most 4,000 characters); the three answers; the domain of the owner's sign-up address (never the address); the sending and link domains and their age; earlier import checks (file names, statistics); counts of contacts from other sources; the first campaign's or flow's messages, if any.
- **Import**: the file name, the column names and the statistics above.
- **Content**: the account's name, website domain and own domains; the campaign or flow name; each message's subject, preview text, text and links.

Never sent: a contact's address, name or phone number, or any row of an import.

The customer's material is marked as data the model must not take instructions from, and every answer is checked against the expected form. The model answers with a verdict and fixed reason codes; customers see only DumbMail's own sentence for each code, and the model's own words go only to the operator. The model, its verdict and token counts are recorded on the screening.

## For the operator

All with `Authorization: Bearer $ADMIN_TOKEN`, on the hosted service only (elsewhere they answer 404).

- `GET /admin/screening`: accounts that are pending (with what they're waiting for), in review or blocked, or have held content, newest first, each with its screenings (reasons, signals, source, model), the three answers, and `checked_at` (when this list was read).
- `POST /admin/screening/decide` with `{"account_id": "…", "decision": "approve" | "review" | "block", "note": "…", "seen_at": "<checked_at>", "affiliated_brands": ["paypal"]}`. Approving also lifts an email pause, in the same statement, so one call lets the account send again. The decision is refused (409) if the account changed after you looked (`seen_at`) or while it was being written, so it never erases a newer pause or check. The note is shown to the customer. `affiliated_brands` (optional) marks the brands the account really belongs to.
- `POST /admin/screening/pass` with `{"screening_id": "…"}`: let a held campaign or flow go. The owner is emailed to send it again.

Every decision is recorded as a screening with source `operator`.
