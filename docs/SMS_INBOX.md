# SMS inbox: two-way texting

When someone texts your DumbMail number, the text lands in the **Inbox**. You can read the whole
conversation, reply, close it when it's handled, and set up keywords like "text JOIN to subscribe".

Everything here works in the dashboard (Inbox), the REST API (`/api/inbox/...`) and the MCP server.

## What happens to an incoming text

1. **The provider's signature is checked.** Telnyx signs each webhook with Ed25519
   (`telnyx-signature-ed25519` over `{telnyx-timestamp}|{body}`; webhooks older than 5 minutes are
   refused). Twilio signs with `X-Twilio-Signature` (HMAC-SHA1 of the exact URL and the sorted
   parameters, keyed by the auth token). Where a key is configured, a webhook that fails the check
   is refused. See [Verifying inbound texts](#verifying-inbound-texts).
2. The receiving number must belong to exactly one account. If it belongs to none, or to several,
   nothing is stored (an opt-out is still applied in each account it could be for). A text without
   the provider's message id is refused the same way.
3. **Opt-out is decided first**, before anything else looks at the text, and it errs toward opting
   out (see [Opting out](#opting-out)). The text is stored, and its effect on consent (an opt-out, or
   START lifting one) is written in the same transaction. A provider retry of the same text is
   recognized by its message id and not applied twice; if storing failed, the retry applies it.
4. It's matched to a profile by phone number. A number we haven't seen gets a new profile with
   SMS consent **never subscribed**: texting you isn't consent to marketing.
5. Then, only for a verified text from a number that hasn't opted out: HELP gets its answer; a
   "Y" answers a double opt-in; otherwise a **Received SMS Reply** event is recorded (properties:
   `text`, `keyword` when a keyword matched, `media_count`, `to`, `inbound_message_id`) and a keyword,
   or the default/away reply, runs; finally flows waiting for a reply wake up.

Pictures are copied into DumbMail (PNG, JPEG, GIF and WebP up to 5 MB) because the providers' links
expire. Twilio's links need the account's credentials: DumbMail sends them only to
`api.twilio.com`, only for that Twilio account's media of that verified message, and fetches the
file Twilio redirects to without them. Other files (video, contact cards) are shown as the
provider's link, except Twilio's, which can't be opened from a browser. Copied pictures are only
visible to signed-in members of the account.

## Opting out

A text opts the number out when, after trimming spaces and punctuation, in any case:

- it is just one of **STOP, STOPALL, STOP ALL, UNSUBSCRIBE, CANCEL, END, QUIT, REVOKE, OPTOUT,
  OPT OUT**, or
- it contains, as whole words, **STOP, UNSUBSCRIBE, OPT OUT, REMOVE ME, DO NOT TEXT, DON'T TEXT** or
  **TAKE ME OFF** ("Please stop texting me"). CANCEL, END and QUIT count only on their own, because
  they're common in ordinary sentences ("Can I cancel my order?"), or
- the provider says it handled the text as an opt-out (Twilio's `OptOutType: STOP`, Telnyx's
  `autoresponse_type: STOP`), whatever the words.

An opt-out never triggers a keyword, an automatic reply or a flow, shows in the thread as an
opt-out, and records **Unsubscribed from SMS**. It also cancels the number's queued texts and any
pending double opt-in. Opt-outs are honored even from texts that couldn't be verified.

The number is then **opted out at the carrier level**: nothing can be sent to it from the account
(HELP answers excepted). This is kept separate from the profile's marketing consent. Every text
DumbMail sends (campaigns, flows, replies, automatic texts) is checked against it in the database
immediately before it goes to the provider, under the same lock an incoming opt-out takes, so an
opt-out that arrived first always wins.

Only the provider's own opt-back-in words lift it: **START** and **UNSTOP** (and **YES** on Twilio,
except on toll-free numbers, where Twilio documents that YES doesn't work), or the provider saying
it handled the text as START. As before the inbox, START also marks the person subscribed to texts.
Anything else from an opted-out number, a sign-up keyword such as JOIN included, does nothing and
sends nothing; the thread shows "Texted JOIN while opted out".

People who are not opted out and already have a profile can still text START, UNSTOP, YES or
SUBSCRIBE to subscribe, as before the inbox.

## HELP

**HELP** and **INFO** get one fixed, non-promotional answer, at most once an hour per number:

> {program name}: For help, contact {support contact}. Msg & data rates may apply. Reply STOP to cancel.

It also goes to a number that opted out (it's administrative; the provider still applies its own
block). It never starts a keyword, an automatic reply or a flow. Who answers is the `help_replies`
setting:

- `auto` (default): Twilio answers HELP itself on long-code and toll-free numbers, so DumbMail
  doesn't; Telnyx documents no default HELP answer for those numbers, so DumbMail answers. A text
  the provider says it already answered as HELP isn't answered again.
- `dumbmail`: DumbMail always answers. `provider`: DumbMail never does.

## Verifying inbound texts

Keywords, automatic replies, HELP answers, the 24-hour reply window and waking flows all need
verified texts. Without a signing key the inbox still keeps every text and honors every opt-out,
but an unverified text can't subscribe anyone, open a reply window, get an automatic reply or
start or wake a flow, and the thread marks it **Not verified**. The settings response's
`verification` says whether texts are verified and where the key comes from:

- The platform's Telnyx: the server's `TELNYX_PUBLIC_KEY` (Telnyx portal → Keys & Credentials → Public Key).
- The platform's Twilio: the server's `TWILIO_AUTH_TOKEN`.
- Your own Telnyx (connected in Settings → SMS): save its public key as `telnyx_public_key` in the
  inbox settings. It's stored encrypted with your Telnyx connection and shown only as a hint.
- Your own Twilio: the auth token you connected with is used.

## Conversations

One conversation per person who has texted in. Each shows their last message, an unread count,
and whether it's open or closed. Closing one hides it from the open list; it reopens by itself when
they text again. A thread shows the full history with that person: their texts, your replies,
automatic replies, and every campaign and flow text they were sent (texts sent before the inbox
existed show without their words, which weren't kept then).

- `GET /api/inbox/conversations?status=open|closed|all&unread=true&q=…&limit=50&offset=0`
- `GET /api/inbox/conversations/{id}?before=<time>&limit=50`: newest first; `next_before` pages back.
  Also returns `reply` (`allowed`, `reason`, `window_ends_at`, `to`, `from`). Each conversation has
  `opted_out`, `their_number` and `our_number`.
- `POST /api/inbox/conversations/{id}/read`
- `POST /api/inbox/conversations/{id}/status` with `{"status": "closed"}` or `"open"`
- `GET /api/inbox/unread`: `{conversations, texts}`
- `GET /api/inbox/profiles/{profile_id}`: `{conversation_id}` (null if they never texted in)
- MCP: `list_conversations`, `get_conversation`, `close_conversation` (`reopen: true` to reopen),
  `mark_conversation_read`

## Replying

`POST /api/inbox/conversations/{id}/reply` with `{"body": "…", "confirm": true}`, or the MCP tool
`send_sms_reply`. Without `confirm: true` nothing is sent: you get the text exactly as it would go
out (personalization filled in, your brand name and "Reply STOP to opt out." added where carriers
require them) and its segment count.

The rules, checked when you compose and again right before the text goes to the provider:

- A reply goes **to the number that texted** and **from the number they texted**, from their
  latest text. Editing the profile's phone number doesn't move it.
- **Never** to a number that opted out (until it texts START again), or to someone unsubscribed.
- People **subscribed** to texts on that number can be replied to any time.
- Anyone else can be replied to for **24 hours after their last verified text**, timed from when
  the provider received it. After that the reply box says why it's closed.
- Quiet hours don't hold a reply: they just texted you.
- Replies are billed like any text on hosted DumbMail, and count as a received text in segments and
  reports ("Received SMS"). They aren't marketing: they don't count for smart sending.

**Saved replies** are canned responses with personalization tags such as
`{{ first_name|default('there') }}`, filled in for each person when sent.
`GET/POST /api/inbox/saved-replies`, `PATCH/DELETE /api/inbox/saved-replies/{id}`; MCP
`list_saved_replies`, `upsert_saved_reply`, `delete_saved_reply` (`confirm: true`).

## Keywords

A keyword is one word of letters and digits (case doesn't matter), with up to 10 aliases.

| Field | Meaning |
| --- | --- |
| `keyword` | e.g. `JOIN` |
| `aliases` | other words that do the same, e.g. `["VIP", "DEALS"]` |
| `match_type` | `exact` (the whole text is the word) or `starts_with` ("PROMO codes?"); sign-up keywords must be `exact` |
| `subscribe` | subscribe the sender to SMS marketing |
| `double_opt_in` | with `subscribe`: ask them to reply Y first |
| `list_id` | also add them to this list |
| `reply` | the text sent back (for sign-ups, the start of the confirmation) |
| `enabled` | turn it off without deleting it; turning one on needs a `support_contact` |

Reserved words can't be keywords or aliases: STOP, STOPALL, UNSUBSCRIBE, CANCEL, END, QUIT, OPTOUT,
REVOKE, START, UNSTOP, YES, SUBSCRIBE, HELP, INFO and Y.

`GET/POST /api/inbox/keywords`, `PATCH/DELETE /api/inbox/keywords/{id}`; MCP `list_sms_keywords`,
`upsert_sms_keyword` (no `id` creates; with `id`, fields left out stay the same),
`delete_sms_keyword` (`confirm: true`).

### "Text JOIN to subscribe"

A keyword with `subscribe: true` needs a **program name** and **message frequency** in the inbox
settings, because the confirmation must carry them. The confirmation is always:

> {program name}: {your reply, or "Thanks! You're signed up for texts."} {message frequency}. Msg & data rates may apply. Reply HELP for help, STOP to cancel.

With `double_opt_in`, they first get:

> {program name}: Reply Y to confirm you want recurring automated marketing texts from us. {message frequency}. Msg & data rates may apply. Reply HELP for help, STOP to cancel. Consent is not a condition of purchase.

Nobody is subscribed until the confirmation, with the full disclosure, has been **accepted by the
provider**. If it can't go (a limit was reached, the provider refused it, they opted out
meanwhile), they aren't subscribed. With double opt-in, a **Y** or **YES** counts only if the
question actually went out to that number, from the number they texted, within the last 7 days,
and the keyword is still on and still subscribes; then the confirmation goes, and they're
subscribed once it's accepted.

Each sign-up keeps a consent record: the exact disclosure text sent, the keyword and its version
(it goes up on every edit), both phone numbers and the provider's message ids (the sign-up text,
the Y, the question and the confirmation). It's also recorded as a **Subscribed to SMS** event with
`method` (`keyword` or `double_opt_in`). The program name, message frequency and support contact
can't be cleared while keywords use them.

## Automatic replies

Set in the inbox settings:

- **Default reply**: for texts no keyword matched.
- **Away reply**: instead of the default reply outside opening hours (`open_hour` to `close_hour`
  on `open_days`, in the account's time zone).

Automatic replies never answer an opt-out, START or HELP, never go to a number that opted out, go
out only within 24 hours of the text they answer (checked again when they send), and are limited:

- per person: the default and away reply at most once every 12 hours, and at most 5 automatic
  texts of any kind an hour, so two automated numbers can't text each other forever;
- per account and per receiving number, every hour: `auto_budget_account` (default 200, at most
  2,000) and `auto_budget_number` (default 100, at most 1,000) automatic texts of every kind
  (keyword answers, sign-up texts, default and away replies, HELP answers). When one runs out,
  automatic texts stop for the rest of the hour and the inbox shows it (`auto_paused`,
  `auto_budget_hit_scope`).

Every automatic text is written down before it's sent, and its id is the id of the message it goes
out as, so it can go at most once even if its job runs twice.

## Settings

`GET/PATCH /api/inbox/settings`, MCP `get_sms_inbox_settings` / `update_sms_inbox_settings`:
`program_name`, `message_frequency`, `support_contact`, `help_replies` (`auto`, `dumbmail`,
`provider`), `auto_budget_account`, `auto_budget_number`, `telnyx_public_key` (write-only),
`default_reply_enabled`, `default_reply`, `away_reply_enabled`,
`away_reply`, `open_hour`, `close_hour`, `open_days` (1 = Monday … 7 = Sunday), `notify_user_ids`.

**Email digests**: members in `notify_user_ids` get one email listing who texted, at most every 30
minutes, not one email per text. The settings response lists the account's `members`.

## Flows

**Trigger, "replied to a text"**: a metric trigger on Received SMS Reply. To start only on a
keyword or certain words, add a filter:

```json
{"type": "metric", "metric": "Received SMS Reply", "filters": [{"field": "keyword", "op": "eq", "value": "JOIN"}]}
{"type": "metric", "metric": "Received SMS Reply", "filters": [{"field": "text", "op": "contains", "value": "pizza"}]}
```

**Step, "wait for a reply"**: waits until the person texts back or the time runs out, then goes
down `yes` (they replied) or `no` (they didn't). Only verified texts received before the deadline
count, however late the step runs; opt-outs, START and HELP don't count. The longest wait is 30 days.

```json
{"id": "w1", "type": "wait_for_reply", "amount": 2, "unit": "days", "yes": "thanks", "no": "nudge"}
```

## For smart sending

Each SMS's kind is kept in `sms_sent_texts.kind`: `marketing` for campaigns and flows, and `reply`,
`auto_reply`, `keyword_reply`, `confirmation`, `optin_request` or `help` for conversational texts.
Smart sending counts only `marketing` ones, and never delays the others.
