# SMS inbox: two-way texting

When someone texts your DumbMail number, the text lands in the **Inbox**. You can read the whole
conversation, reply, close it when it's handled, and set up keywords like "text JOIN to subscribe".

Everything here works in the dashboard (Inbox), the REST API (`/api/inbox/...`) and the MCP server.

## What happens to an incoming text

1. DumbMail stores it, on Telnyx and Twilio alike, with any pictures (MMS). A provider sending the
   same text twice (a webhook retry) is stored and acted on once.
2. It's matched to a profile by phone number. A number we haven't seen gets a new profile with
   SMS consent **never subscribed**: texting you isn't consent to marketing.
3. STOP, START and HELP work as before:
   - **STOP** (also STOPALL, UNSUBSCRIBE, CANCEL, END, QUIT, OPTOUT, REVOKE) unsubscribes them from texts.
   - **START** (also UNSTOP, YES, SUBSCRIBE) subscribes someone who already had a profile.
   - **HELP** and **INFO** are kept in the thread and get no automatic reply from DumbMail.
   All of these show in the conversation.
4. Any other text records a **Received SMS Reply** event (properties: `text`, `keyword` when a
   keyword matched, `media_count`, `to`, `inbound_message_id`). Flows and segments can use it.
5. Then, in this order: a pending double opt-in "Y", a keyword, or the default/away reply.

Pictures are copied into DumbMail (PNG, JPEG, GIF and WebP up to 5 MB) because the providers' links
expire. Other files (video, contact cards) are shown as the provider's link. Copied pictures are only
visible to signed-in members of the account.

## Conversations

One conversation per person who has texted in. Each shows their last message, an unread count,
and whether it's open or closed. Closing one hides it from the open list; it reopens by itself when
they text again. A thread shows the full history with that person: their texts, your replies,
automatic replies, and every campaign and flow text they were sent (texts sent before the inbox
existed show without their words, which weren't kept then).

- `GET /api/inbox/conversations?status=open|closed|all&unread=true&q=…&limit=50&offset=0`
- `GET /api/inbox/conversations/{id}?before=<time>&limit=50`: newest first; also returns `reply`
  (`allowed`, `reason`, `window_ends_at`).
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

The rules:

- **Never** to someone who texted STOP (until they text START again).
- People **subscribed** to texts can be replied to any time.
- Anyone else can be replied to for **24 hours after their last text**. After that the reply box
  says why it's closed.
- Quiet hours don't hold a reply: they just texted you.
- Replies are billed like any text on hosted DumbMail, and count as a received text in segments and
  reports ("Received SMS").

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
| `match_type` | `exact` (the whole text is the word) or `starts_with` ("JOIN please") |
| `subscribe` | subscribe the sender to SMS marketing |
| `double_opt_in` | with `subscribe`: ask them to reply Y first |
| `list_id` | also add them to this list |
| `reply` | the text sent back (for sign-ups, the start of the confirmation) |
| `enabled` | turn it off without deleting it |

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

Until they reply Y (or YES/START) within 7 days, their SMS consent is **pending** and they get no
marketing texts. When they confirm (or right away without double opt-in), DumbMail records a
**Subscribed to SMS** event with `method` (`keyword` or `double_opt_in`), the keyword, their exact
text, the number they texted and when: that's the consent record. Someone who texted STOP before
can sign up again this way, because they asked to.

## Automatic replies

Set in the inbox settings:

- **Default reply**: for texts no keyword matched.
- **Away reply**: instead of the default reply outside opening hours (`open_hour` to `close_hour`
  on `open_days`, in the account's time zone).

Automatic replies never answer STOP, START or HELP, never go to someone who texted STOP, and are
rate limited per person so two automated numbers can't text each other forever: the default and
away reply at most once every 12 hours, and at most 5 automatic texts of any kind an hour.

## Settings

`GET/PATCH /api/inbox/settings`, MCP `get_sms_inbox_settings` / `update_sms_inbox_settings`:
`program_name`, `message_frequency`, `default_reply_enabled`, `default_reply`, `away_reply_enabled`,
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
down `yes` (they replied) or `no` (they didn't). STOP, START and HELP don't count as replies.

```json
{"id": "w1", "type": "wait_for_reply", "amount": 2, "unit": "days", "yes": "thanks", "no": "nudge"}
```
