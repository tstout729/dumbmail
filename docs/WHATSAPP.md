# WhatsApp

DumbMail sends WhatsApp messages through Meta's WhatsApp Cloud API, using your own WhatsApp Business account. Meta bills your business for WhatsApp messages (see [Billing](#billing)). DumbMail charges nothing per message unless the server sets `PRICE_WHATSAPP`.

WhatsApp works like the other channels. Campaigns and flow steps can send it. Audiences count who can get it. Segments have "can receive WhatsApp". Quiet hours and smart sending apply. Reports show sent, delivered, read, clicked and failed. The inbox shows WhatsApp conversations next to texts.

## Connect your WhatsApp Business account

You need a Meta Business with a WhatsApp Business Account, a phone number registered on the Cloud API, and a Meta app with the WhatsApp product.

1. In Meta Business Settings, create a **system user** and give it your WhatsApp Business Account and the Meta app (full control). Generate a **permanent token** for the app with the `whatsapp_business_messaging` and `whatsapp_business_management` permissions.
2. In WhatsApp Manager, copy the **WhatsApp Business Account ID** and the **phone number ID** (not the phone number itself).
3. In your Meta app, open **App settings → Basic** and copy the **App secret**.
4. In DumbMail, open **Settings → WhatsApp**, paste all four and save. The token and app secret are encrypted. They're never shown again or logged, only the token's last four characters.
5. Click **Check connection**. DumbMail reads the number's display name, quality rating and messaging limit from Meta.
6. In your Meta app, open **WhatsApp → Configuration → Webhook**. Paste the **callback URL** and **verify token** shown in DumbMail's settings, then verify and save. Subscribe to these webhook fields: `messages`, `user_preferences`, `message_template_status_update`, `template_category_update`, `message_template_quality_update` and `phone_number_quality_update`.
7. Each team member who wants WhatsApp tests adds their own phone under **Test numbers** and sends the code shown from that phone to the business number.

Sending turns on only when all four values are saved. The app secret is required because without it DumbMail can't check Meta's webhooks, and opt-outs arrive by webhook. The settings say what's missing, and when the first webhook arrived.

**One verified sender.** The business account and phone number you connect are the sender. Everything that belongs to a sender is bound to it: consent, the 24-hour service windows, queued messages and messages about to go. If you switch to another business account or phone number:

- nobody stays subscribed to WhatsApp (opt-outs stay);
- every service window closes;
- queued messages are skipped ("WhatsApp sender changed"), and a message already on its way to Meta is stopped at its final check;
- a new business account's templates are read again.

Disconnecting does the same, and nothing more can go until you connect again. Changing only the token or app secret keeps the sender.

API: `GET/PATCH/DELETE /api/whatsapp/settings`, `POST /api/whatsapp/settings/check`, `POST /api/whatsapp/settings/verify-token` (a new verify token). MCP: `get_whatsapp_settings`, `update_whatsapp_settings`, `check_whatsapp_connection`, `disconnect_whatsapp` (`confirm: true`).

### Embedded Signup

Meta's Embedded Signup (connecting with a "Continue with Facebook" button) needs DumbMail to be a verified Meta Tech Provider, which it isn't yet. When it is, Embedded Signup will save the same four values in the same place, marked `connected_via: embedded_signup`. Nothing else changes, and accounts connected by hand keep working. It will need: DumbMail's own Meta app approved for `whatsapp_business_messaging` and `whatsapp_business_management`, a Facebook Login for Business configuration, the server exchanging the signup's code for a business token, and subscribing that app to the customer's WhatsApp Business Account's webhooks.

## Templates

Outside the 24 hours after someone last wrote to you, WhatsApp only lets a business send **approved templates**. Campaigns and flow steps always send templates.

- **Sync.** DumbMail reads your templates from Meta when someone who can change settings asks (**Sync templates**, `sync_whatsapp_templates`; it's in the audit log), and Meta's webhooks keep status and category up to date. A template Meta no longer lists is marked deleted.
- **Create.** Make a template in DumbMail and it's submitted to Meta for review (`POST /api/whatsapp/templates`, `create_whatsapp_template` with `confirm: true`; without it you get what would be submitted). Its status starts as pending. Templates made in DumbMail have a text header (optional), a message with variables `{{1}}`, `{{2}}` (one example each, never at the very start or end), a footer, and up to 10 buttons: quick replies and up to 2 link buttons. Link buttons are tracked: the button points at DumbMail's click address, and each message carries its own token, so clicks show in reports and count for attribution. Make templates with media headers in WhatsApp Manager, then sync them; DumbMail sends them with your media link.
- **Categories.** `MARKETING` templates need a WhatsApp opt-in. `UTILITY` templates (order and booking updates) follow the service-message rules below. Meta can change a template's category; the category it has **when a message is handed over** decides, not the one it had when the message was prepared.
- **Paused templates.** When Meta pauses or disables a template for quality (error 132015 or 132016 on a send or a status, or the webhook), DumbMail stops using it and emails the account's owners once. It stays paused until Meta says it's approved again in an event sent after the pause, or a read of your templates made after the pause shows it approved. An older approval delivered late can't reopen it.

## Content for campaigns and flow steps

```json
{
  "template_id": "<DumbMail's id from list_whatsapp_templates>",
  "header": "{{ first_name }}",
  "header_media_url": "https://example.com/banner.jpg",
  "body": {"1": "{{ first_name|default('there') }}", "2": "{{ event.properties.order_id }}"},
  "buttons": {"1": "{{ event.properties.order_id }}"}
}
```

- `body` fills each variable by name: `"1"`, `"2"` and so on, or the template's named variables.
- `header` fills a text header's variable. `header_media_url` is an https link to the image, video or document of a media header.
- `buttons` fills link buttons whose URL ends in a variable, by button position. Buttons DumbMail tracks fill themselves.
- Variables take personalization: profile fields, `person.properties`, and in flows `event.properties` or the custom object. A variable that comes out empty for someone skips that message, so give a default.
- Coupon codes and review links aren't supported in WhatsApp variables. Saving them is refused.
- The editors show a phone-style preview, filled for a sample person (`render_preview` with `channel: whatsapp`).

Flow steps use `{"type": "whatsapp", "content": {...}, "next": ...}`.

## Consent

WhatsApp has its own consent, separate from texts. It belongs to a **phone number** and to the connected sender, not to a profile. If a profile's number changes, the new number has no WhatsApp consent until it opts in, and campaign and flow messages go only to the number the person has when the message is handed over.

- **Subscribed** only by an explicit WhatsApp opt-in:
  - one of the account's **WhatsApp sign-up words** (Settings → WhatsApp → Sign-ups on WhatsApp), typed on WhatsApp as the whole message;
  - a quick-reply button whose payload is one of the account's **opt-in buttons**;
  - a sign-up form's box naming WhatsApp (set `whatsapp_consent_text` on the form; ticking the box subscribes the number, unless it opted out, which only the person can undo on WhatsApp);
  - an API or import call that says the person agreed to WhatsApp marketing, with how they agreed (`PUT /api/whatsapp/profiles/{id}/consent`, `POST /api/whatsapp/consent/import`, MCP `set_whatsapp_consent`, `import_whatsapp_consent`; `evidence` is required to subscribe).
- Texting keywords never subscribe anyone to WhatsApp. Neither does a media caption or a button label that happens to match a word.
- **Unsubscribed** at once by:
  - STOP (and the other opt-out words) sent on WhatsApp;
  - Meta's own "stop marketing messages" choice (the `user_preferences` webhook);
  - Meta's error 131050, on a send or on a later failure status (the opt-out commits with the status);
  - an API call.
- **Order.** Every change carries the time of the event behind it (Meta's time, to the second, or now). A change older than the one in force does nothing, and in the same second an opt-out wins. So a START or sign-up that Meta delivers late can never undo a newer STOP. Each Meta preference event applies once.
- START after STOP subscribes the number again. Meta's "resume marketing messages" is kept in the history but subscribes nobody.
- Texts are never touched by WhatsApp consent, and WhatsApp is never touched by SMS consent.
- Every change is kept with its source (`get_whatsapp_consent` shows the history).

Who gets what:

| Message | Who it reaches |
| --- | --- |
| Marketing template (campaign, flow or inbox reply) | Numbers subscribed to WhatsApp, never people on a marketing hold. |
| Utility template from a flow marked **service messages** | Subscribed numbers, and people who never opted in, including people on a marketing hold (their own order or booking). |
| Utility template anywhere else | Subscribed numbers only. |
| Campaigns | Subscribed numbers only, whatever the template's category. |
| Anything | Never a number that opted out of WhatsApp, or that counts as off WhatsApp. |

An opt-out wins over messages already queued: they're skipped in the same transaction, and the final check before each message goes to Meta reads the opt-out under the same lock.

## Sending

Each message is written with exactly what it sends before the hand-off, so a retry sends the same thing. Right before Meta is called, one transaction holds the number's lock, the sender's settings and the campaign, and checks the live state:

- the account is open and the message was prepared for the connected sender;
- the person exists and still has this number;
- the number hasn't opted out and isn't off WhatsApp;
- the campaign is still sending (a cancel lands first or waits for the hand-off);
- the template is still approved, and its current category decides the rules;
- Meta's back-off is over;
- consent, the marketing hold and quiet hours (marketing), the 24-hour window (free-form replies), or a current member's verified number (tests).

It records the number and the payload it hands over and reads the credentials; the worker sends exactly that.

Meta's answers:

| Meta's answer | What DumbMail does |
| --- | --- |
| 130429, 80007 (rate limits) and Meta outages with an error code | The whole sender waits (1 minute, 5 minutes, 30 minutes, 2 hours) and tries again, then gives up. |
| 131056 (pair rate limit) | Only that person waits; everyone else keeps going. |
| A rate-limit or undeliverable failure reported later by webhook | Tried again the same way, a bounded number of times. |
| 131026 (undeliverable) | Not proof they're off WhatsApp (an old app or terms not accepted can cause it). Tried again after 6 and 24 hours. After 3 undeliverable answers in a row the number counts as off WhatsApp until it writes in. |
| 131050 (stopped marketing messages) | Unsubscribes the number from WhatsApp. Not retried. |
| 132015, 132016 (template paused or disabled) | Stops using the template and emails the owners once. |
| 131048 and other restrictions | Fails it, shows the problem in settings and emails the owners (at most once a day). |
| 131047 (24-hour window closed) | Fails it. |
| 190 and other token or permission errors | Fails it and shows the problem in settings. |
| 80008 (template API rate limit) | Template creation says to try again later. |
| No answer, a gateway error (502, 503 …) or an answer without Meta's error code | Fails it as "no answer from Meta; not sent again". It might have gone, so it's never sent twice. |

Retries of standalone messages run on the background lane, so they never hold up campaigns and flows.

Statuses from the webhook (sent, delivered, read, failed) are matched by Meta's message id. They only move a message forward, so a repeated status changes nothing. Read marks the message opened in reports.

Quiet hours are the texting hours (Settings → Sending), in the recipient's phone number's time zone. Smart sending counts each channel on its own: a WhatsApp marketing message counts for WhatsApp smart sending only, and replies never count.

## Conversations

Messages people send to your WhatsApp number show in **Inbox → WhatsApp**, one conversation per number.

- For 24 hours after someone's last message (WhatsApp's customer service window), anyone who can reply can send them free-form text. The window is checked again right before the reply goes.
- After that, the reply box offers approved templates only, and says why. A marketing template still needs their opt-in, and never goes to someone on a marketing hold.
- Someone who opted out can still be answered in words inside the window. Templates never go to them.
- Texting keywords that add to a list or answer also work on WhatsApp text messages. Automatic answers are limited to 5 per person per hour.
- Each message records **Received WhatsApp Reply** (with `keyword`), so flows can start from it.
- The flow step "wait for a reply" waits for a text by default. Give it `"channel": "whatsapp"` to wait for a WhatsApp reply instead. A reply on the other channel never ends the wait.
- A message without Meta's time for it opens no window and signs nobody up.
- Someone who uses a WhatsApp username without sharing their number is matched through Meta's business-scoped user id, once a signed webhook has tied that id to their number. Until then their messages aren't stored, except STOP, which is kept and applied as soon as we learn the number.

API: `GET /api/whatsapp/conversations`, `GET /api/whatsapp/conversations/{id}`, `POST .../{id}/reply` (`text` or `template`, with `confirm: true`), `POST .../{id}/read`, `POST .../{id}/status`. MCP: `list_whatsapp_conversations`, `get_whatsapp_conversation`, `send_whatsapp_reply`, `close_whatsapp_conversation`.

## Webhook

- `GET /webhooks/whatsapp/{key}` answers Meta's check (`hub.mode=subscribe`, `hub.verify_token`, `hub.challenge`) when the verify token matches.
- `POST /webhooks/whatsapp/{key}` is checked against the `X-Hub-Signature-256` header (HMAC-SHA256 of the raw body with your app secret) before anything in it is read. A bad signature gets a 401.
- Only your WhatsApp Business Account's entries have effects, and message and preference events only for your connected phone number id (`metadata.phone_number_id`). Unknown fields are ignored.
- Meta retries a delivery that doesn't get a 200 for up to 7 days. Inbound messages are stored once by their id. Statuses only move forward. Preference events apply once. Template events apply in Meta's order. A status for a message id DumbMail hasn't recorded yet, less than 2 minutes old, gets a 503 so Meta delivers it again after the send has recorded its id.

## Tests

WhatsApp tests go only to **a current team member's verified number**. A member adds their own phone in Settings → WhatsApp (or `add_whatsapp_test_number`, signed in as themselves), then sends the code shown from that phone, on WhatsApp, to the business number within 30 minutes. That message proves they hold the number. Membership is checked again right before each test goes: a member who leaves gets nothing more. Tests never reach a number that opted out. Campaign tests (`send_test`) and `send_test_whatsapp` both use these numbers. Tests are limited to 50 an hour per account.

## Events and reports

- Events: **Received WhatsApp**, **Read WhatsApp**, **Clicked WhatsApp**, **Received WhatsApp Reply**, **Subscribed to WhatsApp**, **Unsubscribed from WhatsApp**. The website script can't send any of them.
- Segments: `{"type": "can_receive", "channel": "whatsapp"}` and `{"type": "consent", "channel": "whatsapp", "status": "subscribed"}`. Message conditions count WhatsApp too.
- Attribution counts a click on a WhatsApp link button like a click in a text (the SMS click window), and revenue by channel has its own WhatsApp line.
- `GET /api/whatsapp/report?campaign_id=...` or `?flow_id=...` (MCP `get_whatsapp_report`): sent, delivered, read, clicked, failed and skipped, with the top reasons.

## Permissions

| What | Permission |
| --- | --- |
| See settings, templates and test numbers, check the connection | Settings read |
| Change the connection or sign-up settings, sync or create templates | Settings write |
| Add your own test number, send a test | Test send |
| See a person's WhatsApp consent, read conversations | Profiles read |
| Set or import consent | Audience write |
| Reply, close a conversation | Campaign send |
| WhatsApp campaigns and flow steps | The same as email and texts |

Connection changes, template syncs and creation, test numbers, consent changes, tests and replies are in the audit log. Tokens never are.

## Billing

**What Meta charges.** Since July 1, 2025 Meta charges per message, not per conversation, and only for messages it **delivers**; messages people send you are free. Each message has one category: marketing, utility or authentication templates, or service (free-form replies inside the 24-hour window). Rates depend on the category and the recipient's country, and utility and authentication messages get volume discounts. Since October 1, 2026 service messages and utility templates inside the window are charged again, except 1,000 free service messages per business phone number each month. Messages inside a free entry point window (someone who wrote to you from a Click to WhatsApp ad) are free. Meta bills your WhatsApp Business account directly; check Meta's current rate card in your Meta Business settings.

**What DumbMail charges.** Nothing per WhatsApp message unless the server sets `PRICE_WHATSAPP` (dollars per message). Then every WhatsApp send (campaigns, flows, inbox replies, keyword answers) holds that amount from the balance at its final check; a message without enough credit doesn't go. The hold is charged once per message, however many times it's tried:

- when Meta accepts it on DumbMail's own call;
- for a message Meta holds for a quality check or pacing, only when Meta's status says it was sent.

It's given back when Meta refuses the message, when Meta drops a held one, or when Meta never answered. A failure Meta reports later for a message it accepted doesn't give the charge back: those statuses are signed with your own app secret, so they can't be the proof. A hold still open after 3 days is charged if Meta accepted the message, and given back if it didn't.

## What's verified only against Meta's documentation

These were built and tested against Meta's published Cloud API documentation (Graph API v27.0) and a local stand-in. They haven't run against a real WhatsApp Business number yet:

- the send request and its answer (`messages[0].id`, `messages[0].message_status`, `contacts[0].wa_id`), and the error codes in the table above;
- template listing (fields, `paging.cursors.after`), template creation's answer (`id`, `status`, `category`) and Meta's acceptance of templates with tracked link buttons;
- the phone number fields `display_phone_number`, `verified_name`, `quality_rating` and `whatsapp_business_manager_messaging_limit`;
- the webhook payloads: inbound messages (text, quick-reply buttons with payloads, interactive replies, media, `from_user_id`), statuses with `errors[].code` and `pricing.category`, `user_preferences`, `message_template_status_update`, `template_category_update` (`previous_category`, `new_category`), `message_template_quality_update` (`new_quality_score`) and `phone_number_quality_update` (`max_daily_conversations_per_business`, or the older `current_limit`), and `entry[].time`;
- the signature check over the raw body.

A first live check should:

- connect a test number and check it, and verify the webhook in the Meta app;
- sync templates; create a template with a tracked link button and watch it reach approved;
- verify a team member's phone with the code, send a test, and see sent, delivered and read arrive (and the charge, if a price is set);
- tap the link button;
- reply STOP and see consent change; turn off marketing messages in WhatsApp's own settings and see the `user_preferences` stop;
- message the number and reply inside the window; wait 24 hours to see the reply box switch to templates.

## RCS

RCS (rich texts) is an upgrade to texts, not part of WhatsApp. See [RCS](RCS.md) (`dumbmail://docs/rcs`).
