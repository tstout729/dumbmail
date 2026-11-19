# RCS: rich texts, with SMS for everyone else

RCS lets a text arrive from your business's name and logo, with a picture card, buttons that
suggest a reply, and links, on phones that support it (most Android phones, and iPhones on iOS 18
or later where the carrier supports it). Other phones get the same text as a normal SMS.

In DumbMail, RCS is an upgrade to texts, not a separate channel. A text you write once can go
either way. The same rules apply to both: consent, quiet hours, STOP and HELP, the inbox, and
"wait for a reply" in flows.

RCS goes through Telnyx. It works when your texts go through DumbMail's numbers or through your
own Telnyx account. It doesn't work with your own Twilio account.

Everything here works in the dashboard (Settings → SMS → RCS, and the SMS editor), the REST API
(`/api/rcs/...`) and the MCP server.

## Setting it up

1. **Register an RCS agent with Telnyx.** An agent is your business's sender on RCS: its name,
   logo and colours. Telnyx registers your brand, Google reviews it, and the carriers approve the
   agent; this can take days to weeks. With your own Telnyx account you do this in the Telnyx
   portal. On DumbMail's numbers, ask DumbMail support to register the agent for you.
2. **Point the agent at DumbMail.** Set the agent's webhook URL in Telnyx to the RCS webhook URL
   shown in Settings → SMS → RCS (`https://…/webhooks/rcs/<your private address>`). Replies, opt-outs
   and read receipts arrive there. It also proves the agent is yours: the address is private to
   your account.
3. **Save the agent id and press Check.** DumbMail asks Telnyx about the agent. It's confirmed
   when Telnyx shows it enabled, on a messaging profile, with its webhook URL set to your address.
   One agent can be confirmed by only one DumbMail account.
4. **Turn RCS on.** This needs a confirmed agent and Telnyx's webhook signing key (DumbMail's own
   for its numbers; yours under Settings → SMS → inbox settings if you connected your own Telnyx).
   Without the key, DumbMail couldn't trust RCS replies and opt-outs, so it refuses.

RCS is off by default. While it's off, nothing about texts changes. Changing the agent id turns RCS
off until the new agent passes a Check. A Check that finds a definite problem (the agent is gone,
disabled, or pointed elsewhere) also turns RCS off. If you switch texting providers (DumbMail's
numbers to your own Telnyx, or back), RCS stops until you run Check again with the new connection.

## Who gets RCS

A campaign text or flow text goes as RCS when all of these are true:

- RCS is on and the agent is confirmed for your current texting connection;
- Telnyx says the person's phone can get RCS from your agent (Telnyx's capability check);
- the phone can show the parts of the rich version you used (a card needs rich cards; a link
  button needs link buttons; suggested replies need only RCS);
- the text has no MMS image (`media_url`), and it fits RCS's limits (3,072 characters, or 2,000
  with a card).

Everyone else gets the SMS, exactly as before. Replies you send from the inbox, keyword answers and
other conversational texts always go as SMS from your number.

Telnyx's capability answers are kept for 3 days for phones that can get RCS and 1 day for phones
that can't. A text with no saved answer asks Telnyx about its number and waits up to 4 seconds;
without an answer it goes as SMS. When a campaign starts sending, DumbMail also looks up its
numbers in bulk (100 per request) in the background, so most of its texts find an answer ready.

## The rich version

The SMS editor in campaigns and flow steps has an optional **Rich version**. The SMS message stays
required: it's what other phones get, and on RCS it's the message text.

- **Card:** an image (an `https` link to a JPEG, PNG or GIF) and a title (up to 200 characters,
  variables allowed). On RCS the card shows the image and title, with your message text as its
  description, so your brand name and opt-out wording are still there.
- **Suggestions:** up to four buttons, each up to 25 characters. A *reply* sends its label back to
  you as their reply. A *link* opens the web page; the click is tracked like a link in a text.

The preview shows the phone both ways: the RCS form and the SMS form.

In the API and MCP the rich version is `content.rcs`:

```json
{"body": "Acme: our spring sale starts now", "rcs": {
  "card": {"image_url": "https://shop.test/spring.jpg", "title": "Spring sale, {{ first_name|default('friend') }}"},
  "suggestions": [{"type": "url", "text": "Shop now", "url": "https://shop.test/sale"},
                  {"type": "reply", "text": "Not for me"}]}}
```

A campaign or flow with an invalid rich version can't be launched (the problem is shown).

## Sending, and the one fallback

A text going as RCS takes all the normal steps first: the person's consent, quiet hours, the
message record, the rate limit, and the last check and claim under the same lock an inbound STOP
takes. Then, instead of the SMS request:

1. DumbMail records the RCS message before sending (the exact text, numbers and content). It's
   recorded only while RCS is still on for that agent; otherwise the text goes as SMS.
2. DumbMail sends it to Telnyx (`POST /v2/messages/rcs`) with an `Idempotency-Key` of the message
   id and a `webhook_url` of your RCS address.
   - **Telnyx takes it:** it's sent, and billed once at the RCS price.
   - **Telnyx refuses it** with a definite error: nothing went out, so the SMS goes right away.
   - **No clear answer** (a time-out, a gateway error, an error without a code, or "a request with
     this key is still being processed"): the message is marked failed and never sent again, by
     either route, so nobody gets it twice.
3. **Telnyx later reports `sending_failed` or `delivery_failed`** (and the message wasn't delivered
   or read first): the RCS charge is refunded and the SMS fallback is queued, in one step. The
   fallback goes once. Right before it goes, it's checked again like any text, under the same lock:
   the number hasn't opted out, the person still has text consent at the same number, the campaign
   wasn't cancelled, and it's within quiet hours (outside them it waits for the window). It sends
   the same text, from the same number, and is billed as SMS.
4. **`delivery_unconfirmed`** means Telnyx can't tell whether it arrived. It may have, so nothing is
   resent.

So each text is delivered by one route only, and billed once: as RCS, or as SMS when it fell back.

## Replies, opt-outs and read receipts

Telnyx's RCS events come to your RCS webhook address. Each one's Ed25519 signature is checked
(`telnyx-signature-ed25519` over `{telnyx-timestamp}|{body}`, at most 5 minutes old) before
anything in it is read; unsigned or badly signed events are refused.

- **Replies** (typed, or a tap on a suggested reply) arrive in the inbox as texts to your number,
  so STOP, HELP, keywords, auto-replies and "wait for a reply" work exactly as for SMS. A tap
  arrives as the button's label. Your answers go as SMS.
- **STOP over RCS** stops all texts to that number, RCS and SMS, like a texted STOP.
- A message for an agent that isn't your confirmed agent is only honoured as an opt-out.
- **Delivered** marks the text delivered. Statuses only move forward, so a repeated or late event
  changes nothing.
- **Read receipts** record a `Read SMS` event (properties `message_id`, `campaign_id`, `flow_id`,
  `rcs: true`). Only DumbMail records it: it can't be sent from a browser or the API. Phones don't
  always send read receipts, so "read" undercounts.
- The text's `Received SMS` event has `rcs: true` when it went as RCS.

## Billing

On the hosted service, each RCS message costs `PRICE_RCS` (set by the operator; by default the
price of one SMS segment), charged from your balance when Telnyx takes it and shown as "RCS
messages sent". A long text that would be several SMS segments is still one RCS message. When an RCS
message falls back, its charge is refunded ("RCS message not delivered: refunded") and the text is
billed as SMS. Texts through your own Telnyx account aren't billed by DumbMail. Telnyx bills RCS per
message on its own account; see Telnyx's messaging pricing.

Before a campaign launches, the cost check uses whichever is larger for each text: the SMS cost or
the RCS price. A single text is sent only when the balance covers the larger too.

## Reports

Campaign and flow reports show texts that went as RCS and as SMS side by side: sent, delivered and
clicked, and read for RCS ("read (RCS)"). A fallback text counts as SMS. The totals are the usual
campaign or flow numbers.

## API and MCP

| What | REST | MCP |
| --- | --- | --- |
| Read the settings (agent, confirmed, on, webhook URL, prices) | `GET /api/rcs/settings` | `get_rcs_settings` |
| Save the agent id (`agent_id`, null forgets it) and turn RCS on or off (`enabled`) | `PATCH /api/rcs/settings` | `update_rcs_settings` |
| Check the agent with Telnyx | `POST /api/rcs/settings/check` | `check_rcs_agent` |
| RCS and SMS results for a campaign or flow | `GET /api/rcs/report?campaign_id=…` or `?flow_id=…` | `get_rcs_report` |

Reading the settings needs account read; changing them and the Check need settings write (both are
recorded in the audit log); the report needs messaging read. The webhook
(`POST /webhooks/rcs/{token}`) is public and authenticated by Telnyx's signature.

## What's verified only against Telnyx's documentation

These were built and tested against Telnyx's published RCS documentation (the API reference for
sending, agents and capabilities, and the "Receiving RCS webhooks" guide) and a local stand-in.
They haven't run against a real RCS agent yet:

- the agent lookup `GET /v2/messaging/rcs/agents/{id}` and its fields `agent_id`, `agent_name`,
  `enabled`, `profile_id` and `webhook_url`;
- the capability checks (`GET /v2/messaging/rcs/capabilities/{agent_id}/{phone_number}` and
  `POST /v2/messaging/rcs/bulk_capabilities`) and that `features` is empty or null for a phone
  without RCS; how Telnyx answers for a number it can't look up isn't documented (DumbMail treats a
  definite 4xx as "no RCS" for a day, and no answer as "SMS this time");
- the send request (`agent_id`, `to`, `messaging_profile_id`, `type`, `webhook_url`,
  `agent_message.content_message` with `text`, `suggestions` or a `rich_card.standalone_card`) and
  its answer (`data.id`, `data.type`), and the `Idempotency-Key` header;
- the webhook payloads: `message.received` (`from.phone_number`, `to[0].agent_id`, `body.text`,
  `body.suggestion_response.text`, `body.user_file`), `message.sent`, `message.finalized` and
  `message.read` (`id`, `from.agent_id`, `to[0].status`), and the statuses `delivered`, `read`,
  `sending_failed`, `delivery_failed` and `delivery_unconfirmed`;
- that per-message `webhook_url` overrides the messaging profile's URL for status events.

A first live check should:

- register a test agent, set its webhook URL, and run Check;
- send a test text to a team member's RCS phone and to an iPhone without RCS, and see RCS and SMS
  arrive (and the charges);
- tap a suggested reply and a link; reply STOP over RCS and see texts stop;
- turn the phone off for longer than the carrier keeps messages, to see whether Telnyx reports
  `delivery_failed` (the fallback) or `delivery_unconfirmed` (no fallback);
- read a message and see `Read SMS`.
