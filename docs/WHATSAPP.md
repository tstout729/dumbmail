# WhatsApp

DumbMail sends WhatsApp messages through Meta's WhatsApp Cloud API, using your own WhatsApp Business account. Meta bills your business for WhatsApp conversations as usual. DumbMail charges nothing per message unless the server sets `PRICE_WHATSAPP`.

WhatsApp works like the other channels. Campaigns and flow steps can send it. Audiences count who can get it. Segments have "can receive WhatsApp". Quiet hours and smart sending apply. Reports show sent, delivered, read, clicked and failed. The inbox shows WhatsApp conversations next to texts.

## Connect your WhatsApp Business account

You need a Meta Business with a WhatsApp Business Account, a phone number registered on the Cloud API, and a Meta app with the WhatsApp product.

1. In Meta Business Settings, create a **system user** and give it your WhatsApp Business Account and the Meta app (full control). Generate a **permanent token** for the app with the `whatsapp_business_messaging` and `whatsapp_business_management` permissions.
2. In WhatsApp Manager, copy the **WhatsApp Business Account ID** and the **phone number ID** (not the phone number itself).
3. In your Meta app, open **App settings → Basic** and copy the **App secret**.
4. In DumbMail, open **Settings → WhatsApp**, paste all four and save. The token and app secret are encrypted. They're never shown again or logged, only the token's last four characters.
5. Click **Check connection**. DumbMail reads the number's display name, quality rating and messaging limit from Meta.
6. In your Meta app, open **WhatsApp → Configuration → Webhook**. Paste the **callback URL** and **verify token** shown in DumbMail's settings, then verify and save. Subscribe to these webhook fields: `messages`, `message_template_status_update`, `message_template_quality_update`, `phone_number_quality_update` and `user_preferences`.
7. Add your team's own phone numbers as **test numbers**. WhatsApp tests only go to these.

The settings say when the first webhook arrived, so you can see it works.

API: `GET/PATCH/DELETE /api/whatsapp/settings`, `POST /api/whatsapp/settings/check`, `POST /api/whatsapp/settings/verify-token` (a new verify token). MCP: `get_whatsapp_settings`, `update_whatsapp_settings`, `check_whatsapp_connection`, `disconnect_whatsapp` (`confirm: true`).

### Embedded Signup

Meta's Embedded Signup (connecting with a "Continue with Facebook" button) needs DumbMail to be a verified Meta Tech Provider, which it isn't yet. When it is, Embedded Signup will save the same four values in the same place, marked `connected_via: embedded_signup`. Nothing else changes, and accounts connected by hand keep working. It will need: DumbMail's own Meta app approved for `whatsapp_business_messaging` and `whatsapp_business_management`, a Facebook Login for Business configuration, the server exchanging the signup's code for a business token, and subscribing that app to the customer's WhatsApp Business Account's webhooks.

## Templates

Outside the 24 hours after someone last wrote to you, WhatsApp only lets a business send **approved templates**. Campaigns and flow steps always send templates.

- **Sync.** DumbMail reads your templates from Meta when you ask (**Sync templates**, `sync_whatsapp_templates`) and whenever Meta's template status webhook arrives. Each template keeps its name, language, category, status, parts and variables. A template Meta no longer lists is marked deleted.
- **Create.** Make a template in DumbMail and it's submitted to Meta for review (`POST /api/whatsapp/templates`, `create_whatsapp_template` with `confirm: true`; without it you get what would be submitted). Its status starts as pending. Templates made in DumbMail have a text header (optional), a message with variables `{{1}}`, `{{2}}` (one example each, never at the very start or end), a footer, and up to 10 buttons: quick replies and up to 2 link buttons. Link buttons are tracked: the button points at DumbMail's click address, and each message carries its own token, so clicks show in reports and count for attribution. Make templates with media headers in WhatsApp Manager, then sync them; DumbMail sends them with your media link.
- **Categories.** `MARKETING` templates need a WhatsApp opt-in. `UTILITY` templates (order and booking updates) follow the service-message rules below.
- **Paused templates.** When Meta pauses or disables a template for quality (error 132015 or 132016, or the webhook), DumbMail stops using it and emails the account's owners. Messages using it are skipped until it's approved again.

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

WhatsApp has its own consent, separate from texts. It belongs to a **phone number**, not a profile. If a profile's number changes, the new number has no WhatsApp consent until it opts in.

- **Subscribed** only by an explicit WhatsApp opt-in:
  - a sign-up keyword sent on WhatsApp (your texting keywords work on WhatsApp too);
  - an API or import call that says the person agreed to WhatsApp marketing, with how they agreed (`PUT /api/whatsapp/profiles/{id}/consent`, `POST /api/whatsapp/consent/import`, MCP `set_whatsapp_consent`, `import_whatsapp_consent`; `evidence` is required to subscribe).
- **Unsubscribed** at once by:
  - STOP (and the other opt-out words) sent on WhatsApp;
  - Meta's own "stop marketing messages" choice (the `user_preferences` webhook);
  - Meta's error 131050 on a send;
  - an API call.
- Texts are never touched by WhatsApp consent, and WhatsApp is never touched by SMS consent.
- START after STOP subscribes the number again. Meta's "resume marketing messages" is kept in the history but subscribes nobody.
- Every change is kept with its source (`get_whatsapp_consent` shows the history).

Who gets what:

| Message | Who it reaches |
| --- | --- |
| Marketing template (campaign or flow) | Numbers subscribed to WhatsApp. |
| Utility template from a flow marked **service messages** | Subscribed numbers, and people who never opted in, including people on a marketing hold (their own order or booking). |
| Utility template anywhere else | Subscribed numbers only. |
| Campaigns | Subscribed numbers only, whatever the template's category. |
| Anything | Never a number that opted out of WhatsApp, or that Meta said isn't on WhatsApp. |

An opt-out wins over messages already queued: they're skipped in the same transaction, and the last check before each message goes to Meta reads the opt-out under the same lock.

## Sending

Each message is written with exactly what it sends before the hand-off, so a retry sends the same thing. Right before Meta is called, one transaction holding the number's lock checks the account, the person, the opt-out, whether the number is on WhatsApp, the campaign, the template's approval, consent and quiet hours.

Meta's answers:

| Meta error | What DumbMail does |
| --- | --- |
| 131026 (not a WhatsApp number) | Marks the number as not on WhatsApp. Nothing more goes to it until it writes in. Not retried. |
| 131050 (stopped marketing messages) | Unsubscribes the number from WhatsApp. Not retried. |
| 130429, 131056, 80007 (rate limits) and Meta outages | Waits (1 minute, 5 minutes, 30 minutes, 2 hours) and tries again, then gives up. |
| 132015, 132016 (template paused or disabled) | Stops using the template and emails the owners. |
| 131047 (24-hour window closed) | Fails it. |
| 190 and other token or permission errors | Fails it and shows the problem in settings. |
| No answer at all | Fails it as "no answer from Meta; not sent again". It might have gone, so it's never sent twice. |

Statuses from the webhook (sent, delivered, read, failed) are matched by Meta's message id. They only move a message forward, so a repeated status changes nothing. Read marks the message opened in reports.

Quiet hours are the texting hours (Settings → Sending), in the recipient's phone number's time zone. Smart sending counts WhatsApp marketing messages, not replies.

## Conversations

Messages people send to your WhatsApp number show in **Inbox → WhatsApp**, one conversation per number.

- For 24 hours after someone's last message (WhatsApp's customer service window), anyone who can reply can send them free-form text.
- After that, the reply box offers approved templates only, and says why. A marketing template still needs their opt-in.
- Someone who opted out can still be answered in words inside the window. Templates never go to them.
- Keywords work as for texts. A sign-up keyword subscribes the number to WhatsApp, not texts. Automatic answers are limited to 5 per person per hour.
- Each message records **Received WhatsApp Reply** (with `keyword`), so flows can start from it. The "wait for a reply" flow step counts WhatsApp replies too.

API: `GET /api/whatsapp/conversations`, `GET /api/whatsapp/conversations/{id}`, `POST .../{id}/reply` (`text` or `template`, with `confirm: true`), `POST .../{id}/read`, `POST .../{id}/status`. MCP: `list_whatsapp_conversations`, `get_whatsapp_conversation`, `send_whatsapp_reply`, `close_whatsapp_conversation`.

## Webhook

- `GET /webhooks/whatsapp/{key}` answers Meta's check (`hub.mode=subscribe`, `hub.verify_token`, `hub.challenge`) when the verify token matches.
- `POST /webhooks/whatsapp/{key}` is checked against the `X-Hub-Signature-256` header (HMAC-SHA256 of the raw body with your app secret) before anything in it is read. A bad signature gets a 401.
- Only entries for your WhatsApp Business Account and messages for your phone number id are used. Unknown fields are ignored.
- Meta retries a delivery that doesn't get a 200 for up to 7 days. Inbound messages are stored once by their id. Statuses and consent changes are idempotent. A status for a message id DumbMail hasn't recorded yet, less than 2 minutes old, gets a 503 so Meta delivers it again after the send has recorded its id.

## Tests

WhatsApp tests go only to the team's test numbers (Settings → WhatsApp), never to a number that opted out. Campaign tests (`send_test`) and `send_test_whatsapp` both use them. Tests are limited to 50 an hour per account.

## Events and reports

- Events: **Received WhatsApp**, **Read WhatsApp**, **Clicked WhatsApp**, **Received WhatsApp Reply**, **Subscribed to WhatsApp**, **Unsubscribed from WhatsApp**. The website script can't send any of them.
- Segments: `{"type": "can_receive", "channel": "whatsapp"}` and `{"type": "consent", "channel": "whatsapp", "status": "subscribed"}`. Message conditions count WhatsApp too.
- Attribution counts a click on a WhatsApp link button like a click in a text (the SMS click window).
- `GET /api/whatsapp/report?campaign_id=...` or `?flow_id=...` (MCP `get_whatsapp_report`): sent, delivered, read, clicked, failed and skipped, with the top reasons.

## Permissions

| What | Permission |
| --- | --- |
| See settings and templates, check the connection, sync templates | Settings read |
| Change the connection, the test numbers, create templates | Settings write |
| Send a test | Test send |
| See a person's WhatsApp consent, read conversations | Profiles read |
| Set or import consent | Audience write |
| Reply, close a conversation | Campaign send |
| WhatsApp campaigns and flow steps | The same as email and texts |

Connection changes, template creation, consent changes, tests and replies are in the audit log. Tokens never are.

## Billing

Meta bills your WhatsApp Business account for its conversations. DumbMail charges nothing per WhatsApp message unless the server sets `PRICE_WHATSAPP` (dollars per message). Then each message is charged once from the balance, when it's sent, like texts.

## What's verified only against Meta's documentation

These were built and tested against Meta's published Cloud API documentation (Graph API v27.0) and a local stand-in. They haven't run against a real WhatsApp Business number yet:

- the send request and its answer (`messages[0].id`, `contacts[0].wa_id`), and the error codes in the table above;
- template listing (fields, `paging.cursors.after`), template creation's answer (`id`, `status`, `category`) and Meta's acceptance of templates with tracked link buttons;
- the phone number fields `display_phone_number`, `verified_name`, `quality_rating` and `whatsapp_business_manager_messaging_limit`;
- the webhook payloads: inbound messages (text, quick-reply buttons, interactive replies, media), statuses with `errors[].code` and `pricing.category`, `message_template_status_update`, `message_template_quality_update` (`new_quality_score`), `phone_number_quality_update` (`current_limit`) and `user_preferences`;
- the signature check over the raw body.

A first live check should: connect a test number and check it; verify the webhook in the Meta app; sync templates; create a template with a tracked link button and watch it reach approved; send a test to a team number and see sent, delivered and read arrive; tap the link button; reply STOP and see consent change; message the number and reply inside the window; and wait 24 hours to see the reply box switch to templates.

People who use a WhatsApp username without sharing their phone number (Meta's BSUIDs) can't be stored yet: their messages are logged and dropped.

## RCS

RCS (rich texts on Android) isn't part of DumbMail yet. Telnyx documents a public RCS API (`POST /v2/messages/rcs` with a registered RCS agent, a rich card and an `sms_fallback`), so it can be built as an upgrade to texts. It would need: an RCS agent registered with Google and approved by carriers (the account's agent id in settings, switched off until then); RCS content on texts (a card with an image, title, description and up to four suggested replies or links); sending through Telnyx's RCS endpoint with SMS fallback, reading `data.type` to know whether it went as RCS or SMS; Telnyx's RCS delivery webhooks; a `PRICE_RCS` price; and reports that show RCS and SMS separately.
