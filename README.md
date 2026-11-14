# Postlark

Open-source email and SMS marketing: campaigns, automated flows, live segments, sign-up forms, revenue attribution and a full MCP server, in one Rust binary with Postgres.

It covers the core of what Klaviyo does at a fraction of the cost. Email goes out through Amazon SES at $0.10 per 1,000. Texts go through Telnyx at about $0.004 per segment plus carrier fees, or through Twilio.

## What's in it

- **Profiles and events.** Profiles are matched on email, phone or external id. Custom properties are merged on update. Track any event (`Placed Order`, `Started Checkout`, …) from your server, the browser snippet or MCP, and dedupe with `unique_id`.
- **Lists and live segments.** Rules cover properties, event counts over time windows (with filters on event properties and values), list membership, consent, and nested AND/OR groups. Every rule compiles to parameterized SQL.
- **Campaigns.** Email or SMS to any mix of lists and segments, with exclusions. You get a live reachable count, test sends, scheduling and one-click send. Reports show deliveries, opens, clicks, top links, unsubscribes, bounces and attributed revenue.
- **Flows.** Triggers are an event (with property filters), joining a list, entering a segment, or a date property such as a birthday. Steps are delays, email, SMS, conditional splits, profile updates, list changes and webhooks. Settings cover re-entry rules, profile filters, exit conditions and smart sending. There are six starter flows: welcome series, abandoned checkout (email, then SMS), browse abandonment, post-purchase, winback and birthday.
- **Email builder.** Block-based content is rendered into responsive, table-based HTML that works in Outlook and Gmail. Personalization uses Jinja tags (`{{ first_name|default('there') }}`). The footer with your mailing address and an unsubscribe link is added automatically. Click tracking, the open pixel and RFC 8058 one-click unsubscribe headers are built in.
- **SMS done right.** Consent is required for every text, and messages only send between 9am and 8pm in the account's time zone. The brand name and "Reply STOP" are added when needed. STOP/START replies update consent. Segment counts let you see the cost before sending.
- **Deliverability.** SES domain authentication (DKIM records shown in the app). Hard bounces and spam complaints suppress the address permanently.
- **Sign-up forms.** Popups, flyouts, embedded forms and hosted pages, with an optional SMS consent checkbox. Installing one takes a single `<script>` tag.
- **Analytics.** Revenue attributed to campaigns and flows (last email open or click within 5 days, or SMS click within 24 hours), open, click, unsubscribe and bounce rates, a daily series in your time zone, and a metric explorer with group-by.
- **MCP server.** 45 tools, 3 prompts and 3 reference docs, so Claude, ChatGPT or Cursor can build segments, write and preview emails, create flows, send tests, pull reports and run campaigns. Anything that reaches real people requires `confirm: true` and returns a dry run first.

## Run it

```bash
cp .env.example .env    # set PUBLIC_URL, SECRET_KEY, WEBHOOK_SECRET
docker compose up -d    # dashboard on http://localhost:8080
```

Without Docker you need Rust 1.85+, Node 22+, pnpm and Postgres 15+:

```bash
pnpm -C web install && pnpm -C web build        # dashboard (embedded into the binary)
DATABASE_URL=postgres://localhost/postlark cargo run --release -- serve
```

`EMAIL_PROVIDER=log` and `SMS_PROVIDER=log` print messages instead of sending them, so you can try everything safely.

### Amazon SES

1. In SES, verify your domain from **Settings → Sending** in Postlark. It creates the identity and shows the DKIM CNAME records to add.
2. Ask AWS to move the account out of the SES sandbox.
3. Create a configuration set with an event destination for Bounce, Complaint and Delivery that publishes to an SNS topic. Subscribe `https://YOUR_HOST/webhooks/ses/WEBHOOK_SECRET` over HTTPS (Postlark confirms the subscription automatically).
4. Set `EMAIL_PROVIDER=ses`, `SES_CONFIGURATION_SET`, `AWS_REGION` and credentials (or use an instance role).

### SMS (Telnyx or Twilio)

Register your brand and campaign for 10DLC, buy a number, and point its messaging webhook at `https://YOUR_HOST/webhooks/telnyx/WEBHOOK_SECRET` (or `/webhooks/twilio/…`). Then set `SMS_PROVIDER`, the API credentials and the account's SMS number in **Settings → SMS**.

## MCP

Create an API key in **Settings → Developers**, then:

```bash
# Claude Code
claude mcp add --transport http postlark https://YOUR_HOST/mcp --header "Authorization: Bearer pl_sk_..."
```

For clients that only speak stdio (Claude Desktop, older Cursor builds):

```json
{
  "mcpServers": {
    "postlark": {
      "command": "postlark",
      "args": ["mcp-stdio", "--url", "https://YOUR_HOST"],
      "env": { "POSTLARK_API_KEY": "pl_sk_..." }
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

Browser tracking: `<script async src="https://YOUR_HOST/client/postlark.js?k=PUBLIC_KEY"></script>`, then `postlark.identify({email})` and `postlark.track("Viewed Product", {...})`.

## Development

```bash
# Tests need a Postgres admin URL; each test creates and drops its own database.
TEST_DATABASE_URL=postgres://postgres@localhost:5432/postgres cargo test
cargo clippy --all-targets
pnpm -C web typecheck && pnpm -C web lint
```

## License

AGPL-3.0. You can self-host and modify it freely. If you offer a modified version as a hosted service, you must publish your changes.
