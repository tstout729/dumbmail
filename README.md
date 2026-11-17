# DumbMail

Open-source email and SMS marketing: campaigns, automated flows, live segments, sign-up forms, revenue attribution and a full MCP server, in one Rust binary with Postgres.

It covers the core of what Klaviyo does at a fraction of the cost. Self-host it for free, or use the hosted service and pay only for what you send, with an optional monthly email plan. There's no per-contact fee:

| | Hosted DumbMail | Klaviyo |
| --- | --- | --- |
| Email | $0.40 per 1,000 (first 1,000 a month free) | $150/mo for 10,000 contacts, $720/mo for 50,000 |
| SMS (US, carrier fees included) | $0.015 per segment | about $0.011–0.015 per message, plus carrier fees |
| MMS (US, carrier fees included) | $0.03 | more than an SMS, plus carrier fees |
| Texting number | $2.20 a month, or connect your own Telnyx/Twilio account and texts are free | not listed |

A 10,000-contact store sending about 8 emails per contact a month pays about $32 instead of $150.

Numbers are charged monthly from your credit. If your balance can't cover the rent, the number is marked past due and released after 7 days unless you add credit.

Email goes out through Amazon SES and texts through Telnyx (or your own Telnyx or Twilio account).

## What's in it

- **Profiles and events.** Profiles are matched on email, phone or external id. Custom properties are merged on update. Track any event (`Placed Order`, `Started Checkout`, …) from your server, the browser snippet or MCP, and dedupe with `unique_id`.
- **Lists and live segments.** Rules cover profile properties and location, event counts, totals and averages (lifetime spend, average order) over rolling or calendar windows, first and last time someone did something, engagement, who got, opened or clicked a campaign or flow, form sign-ups, list and segment membership (segments can include other segments, live), consent and reachability, predictions from order history (predicted lifetime value, next order date, churn risk), random samples for holdouts, and nested AND/OR groups. Every rule compiles to parameterized SQL, and every segment's count refreshes on its own.
- **Preference page.** The unsubscribe link lets people pick the lists they want instead of leaving everything; one-click unsubscribe still works.
- **Campaigns.** Email or SMS to any mix of lists and segments, with exclusions. You get a live reachable count, test sends, scheduling and one-click send. Reports show deliveries, opens, clicks, top links, unsubscribes, bounces and attributed revenue.
- **Flows.** Triggers are an event (with property filters), joining a list, entering a segment, or a date property such as a birthday. Steps are delays, email, SMS, conditional splits, profile updates, list changes and webhooks. Settings cover re-entry rules, profile filters, exit conditions and smart sending. There are six starter flows: welcome series, abandoned checkout (email, then SMS), browse abandonment, post-purchase, winback and birthday.
- **Email builder.** Block-based content is rendered into responsive, table-based HTML that works in Outlook and Gmail. Personalization uses Jinja tags (`{{ first_name|default('there') }}`). The footer with your mailing address and an unsubscribe link is added automatically. Click tracking, the open pixel and RFC 8058 one-click unsubscribe headers are built in.
- **SMS done right.** Consent is required for every text, and messages only send between 9am and 8pm in the account's time zone. The brand name and "Reply STOP" are added when needed. STOP/START replies update consent. Segment counts let you see the cost before sending.
- **Deliverability.** SES domain authentication (DKIM records shown in the app). Hard bounces and spam complaints suppress the address permanently. On the hosted service, new accounts warm up: they start at 500 emails a day and the limit doubles each day while bounces and complaints stay low. Campaigns skip people who got 10 or more emails in 180 days without engaging. Imports require confirming the people opted in and skip shared inboxes and throwaway addresses. Double opt-in lists send a confirmation email. Sending follows SES's own rate and waits out its daily quota instead of failing.
- **Sign-up forms.** Popups, flyouts, embedded forms and hosted pages, drawn by one renderer so the editor preview matches the site. Custom fields (birthday, dropdowns, checkboxes and more) save to profile properties, an optional second step asks for a phone number with SMS consent, and forms get full styling, image layouts and uploads, a teaser tab, targeting with a schedule, A/B tests and a shareable preview link. Installing one takes a single `<script>` tag. See [docs/FORMS.md](docs/FORMS.md).
- **Analytics.** Revenue attributed to campaigns and flows (last email open or click within 5 days, or SMS click within 24 hours), open, click, unsubscribe and bounce rates, a daily series in your time zone, and a metric explorer with group-by.
- **Custom domains.** Sending domains get DKIM, a custom MAIL FROM (so SPF and DMARC align) and DMARC, with each DNS record checked live and shown with what DNS actually returns. A branded link domain (e.g. `links.yourbrand.com`) moves tracking links, unsubscribe pages and hosted forms onto your domain, and the TLS certificate is issued automatically.
- **Phone numbers.** Claim a toll-free number in a couple of clicks (the carrier verification form is built in), or connect your own Telnyx or Twilio number. Your credentials are stored encrypted and STOP replies still sync.
- **One snippet for any website.** Paste one `<script>` tag in your site's header or footer (guides for Shopify, WordPress, Squarespace, Wix, Webflow, GTM, Framer and Next.js). It tracks visits, product views, add-to-carts, checkouts and orders by itself on Shopify, WooCommerce and any site with product structured data or Google Analytics ecommerce events. It recognizes people who arrive from your emails and texts or type their email into a form, and shows your popups, flyouts and embedded forms. Targeting covers delay, scroll depth, exit intent, page URL, device and frequency. **Test your setup** opens your live site in test mode and checks each step as you click through it.
- **Pay-as-you-go billing.** Prepaid credit through Stripe Checkout, optional auto-recharge, per-message charges at cost plus markup, a free monthly email allowance, optional monthly email plans (renewed by the server, no Stripe Billing) and a usage ledger. It's turned off entirely when self-hosted.
- **MCP server.** 56 tools, 3 prompts and 3 reference docs, so Claude, ChatGPT or Cursor can build segments, write and preview emails, create flows, send tests, pull reports and run campaigns. Anything that reaches real people requires `confirm: true` and returns a dry run first.

## Run it

```bash
cp .env.example .env    # set PUBLIC_URL, SECRET_KEY, WEBHOOK_SECRET
docker compose up -d    # dashboard on http://localhost:8080
```

Without Docker you need Rust 1.85+, Node 22+, pnpm and Postgres 15+:

```bash
pnpm -C web install && pnpm -C web build        # dashboard (embedded into the binary)
DATABASE_URL=postgres://localhost/dumbmail cargo run --release -- serve
```

`EMAIL_PROVIDER=log` and `SMS_PROVIDER=log` print messages instead of sending them, so you can try everything safely.

### Amazon SES

1. In SES, verify your domain from **Settings → Sending** in DumbMail. It creates the identity and shows the DKIM CNAME records to add.
2. Ask AWS to move the account out of the SES sandbox.
3. Create a configuration set with an event destination for Bounce, Complaint and Delivery that publishes to an SNS topic. Subscribe `https://YOUR_HOST/webhooks/ses/WEBHOOK_SECRET` over HTTPS (DumbMail confirms the subscription automatically).
4. Set `EMAIL_PROVIDER=ses`, `SES_CONFIGURATION_SET`, `AWS_REGION` and credentials (or use an instance role). The IAM user needs `ses:SendRawEmail`, `ses:CreateEmailIdentity`, `ses:GetEmailIdentity`, `ses:DeleteEmailIdentity`, `ses:PutEmailIdentityMailFromAttributes` and `ses:GetAccount` (to follow the account's sending rate).

### Hosted billing (Stripe)

Set `HOSTED=true`, `STRIPE_SECRET_KEY` and `STRIPE_WEBHOOK_SECRET`. Point a Stripe webhook at `https://YOUR_HOST/webhooks/stripe` with the events `checkout.session.completed`, `checkout.session.async_payment_succeeded` and `payment_intent.succeeded`. Prices are set with `PRICE_EMAIL_PER_1000`, `PRICE_SMS_SEGMENT`, `PRICE_MMS`, `PRICE_NUMBER_MONTHLY` and `FREE_EMAILS_PER_MONTH`. Operators can adjust a balance with `dumbmail admin-credit --email owner@brand.com --amount 25`. Email sending pauses automatically for an account whose bounces or spam complaints get high enough to put the shared SES account at risk; set `OPERATOR_ALERT_EMAIL` to be emailed when that happens and lift a pause with `POST /admin/unpause` (see [docs/API.md](docs/API.md#automatic-email-pause)). New accounts start at `WARMUP_START_DAILY` emails a day (500) and grow from there; set a fixed limit for one account with `POST /admin/sending-limit` (see [Sending guardrails](docs/API.md#sending-guardrails)).

### Branded link domains

Hosted installs should set `LINK_URL` to a separate domain used only for tracking, unsubscribe and confirmation links (e.g. `https://dumbmaillinks.com`), so a customer's spam can't get `PUBLIC_URL`'s domain blocklisted. Point that domain at the service and check `https://LINK_HOST/healthz` before setting it; links already sent keep working on `PUBLIC_URL`. A customer's own branded link domain takes priority over it.

On Railway, set `RAILWAY_API_TOKEN` to a project token (Railway provides the project, environment and service ids itself), and customer domains plus their certificates are created through Railway's API. Self-hosters point each customer's CNAME at `PUBLIC_URL`'s host and terminate TLS in their proxy (Caddy's on-demand TLS works well).

### Marketing site and app on separate hosts

By default one host serves everything: the dashboard at `/`, the landing page at `/home`, plus `/pricing`, `/about`, `/privacy` and `/terms`. To put the landing page on its own domain (as dumbmail.ai does, with the dashboard on app.dumbmail.ai), set:

- `PUBLIC_URL` to the app host, e.g. `https://app.example.com`. Links in new email, forms, the website snippet and Stripe return URLs use it.
- `SITE_URL` to the site host, e.g. `https://example.com`. There `/` is the landing page and `/pricing`, `/about`, `/privacy` and `/terms` are served. The API, MCP, webhooks, tracking and unsubscribe links, hosted forms, the website snippet, `/healthz`, Google sign-in and Google's site verification file keep working on it, so nothing already sent or installed breaks. Any other page visit gets a 302 to the same path on `PUBLIC_URL`, and the app host sends those four pages back to `SITE_URL`. Don't list the site host in `REDIRECT_HOSTS`.
- `COOKIE_DOMAIN` to the shared parent domain, e.g. `example.com`, so the session and Google sign-in cookies reach both hosts.
- `GOOGLE_REDIRECT_BASE` to the base of the redirect URI registered on your Google OAuth client, if it isn't `PUBLIC_URL` (e.g. `https://example.com`). The callback finishes signing in on that host and sends people to `PUBLIC_URL`. It needs `COOKIE_DOMAIN` when the hosts differ.

### SMS (Telnyx or Twilio)

Register your brand and campaign for 10DLC, buy a number, and point its messaging webhook at `https://YOUR_HOST/webhooks/telnyx/WEBHOOK_SECRET` (or `/webhooks/twilio/…`). Then set `SMS_PROVIDER`, the API credentials and the account's SMS number in **Settings → SMS**.

## MCP

Create an API key in **Settings → Developers**, then:

```bash
# Claude Code
claude mcp add --transport http dumbmail https://YOUR_HOST/mcp --header "Authorization: Bearer pl_sk_..."
```

For clients that only speak stdio (Claude Desktop, older Cursor builds):

```json
{
  "mcpServers": {
    "dumbmail": {
      "command": "dumbmail",
      "args": ["mcp-stdio", "--url", "https://YOUR_HOST"],
      "env": { "DUMBMAIL_API_KEY": "pl_sk_..." }
    }
  }
}
```

Try: *"Look at my last 90 days of campaigns, find what worked, and draft a campaign for our fall launch to engaged subscribers who haven't bought in 60 days. Send me a test first."*

## API

The HTTP API is documented in [docs/API.md](docs/API.md). Server-side tracking:

```bash
curl https://YOUR_HOST/api/events -H "Authorization: Bearer pl_sk_..." -H "content-type: application/json" \
  -d '{"metric": "Placed Order", "profile": {"email": "ann@example.com"}, "value": 89.5, "unique_id": "order-1001",
       "properties": {"items": ["Wool scarf"]}}'
```

Website: `<script async src="https://YOUR_HOST/onsite.js?k=PUBLIC_KEY"></script>` in the header or footer. Then use `dumbmail.identify({email})` and `dumbmail.track("Viewed Product", {...}, value)`. Embed a form with `<div data-dumbmail-form="FORM_ID"></div>`.

## Development

```bash
# Tests need a Postgres admin URL; each test creates and drops its own database.
TEST_DATABASE_URL=postgres://postgres@localhost:5432/postgres cargo test
cargo clippy --all-targets
pnpm -C web typecheck && pnpm -C web lint
```

## License

AGPL-3.0. You can self-host and modify it freely. If you offer a modified version as a hosted service, you must publish your changes.
