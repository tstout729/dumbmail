# Send times

Campaigns can go out at each person's local time, at each person's best hour, or spread over several hours. Smart sending skips people who just got a message. Texts only go out during the day in the recipient's own time zone.

## Each person's time zone

DumbMail picks one time zone for each person, in this order:

1. **The profile's `timezone` property**, when it's a real time zone name such as `America/New_York` or `Asia/Kolkata`. Sign-up forms fill it from the visitor's browser.
2. **Region**: a US state, Canadian province or Australian state in `region` (or `state`, `province`), by code or name. Australian states need `country` set to `AU`. A state split across zones uses the zone most of its people live in.
3. **US ZIP code** in `zip` (or `postal_code`), when `country` is `US`.
4. **Country** in `country` (a two-letter code or a common name), when the country has one time zone, or one that nearly everyone uses (Spain, Portugal, Mexico, Brazil, Chile, Ecuador, China, New Zealand, Ukraine).
5. **Phone number**: the country calling code when that country has one zone, and in the US and Canada the area code (area codes that cross a zone line are skipped).
6. **The account's time zone** when nothing else tells us.

Check what DumbMail uses for someone with `GET /api/profiles/{id}/timezone` (MCP: `get_profile_timezone`). It returns the zone, its source (`profile`, `region`, `zip`, `country`, `phone` or `account`) and the person's local time.

## Campaign send options

Set `send_options` on a campaign (`POST /api/campaigns`, `PATCH /api/campaigns/{id}`, or MCP `create_campaign`, `update_campaign`, `send_campaign`). `null` or leaving it out sends to everyone at once with no smart sending, as before.

| Field | Used with | Meaning |
| --- | --- | --- |
| `timing` | all | `all_at_once` (default), `local_time`, `best_time` or `gradual` |
| `local_time` | `local_time` | The hour in each person's zone, `HH:MM` (default `10:00`) |
| `late` | `local_time` | For people whose hour already passed when sending starts: `next_day` (default) gets it at that hour tomorrow, `immediately` gets it right away |
| `window_start`, `window_end` | `best_time` | The local hours it may go out in (default `08:00` to `20:00`, at least an hour) |
| `within_hours` | `best_time` | How far ahead of the send time to look (6 to 72, default 24) |
| `spread_hours` | `gradual` | Hours to spread the send over, one equal batch an hour (2 to 72) |
| `smart_sending` | all | `true` skips people who got a message on this channel recently |
| `smart_sending_hours` | all | How recently (1 to 168 hours, default 16) |

Example:

```json
{"send_options": {"timing": "local_time", "local_time": "10:00", "late": "next_day", "smart_sending": true}}
```

### Each person's local time

The campaign goes out at `local_time` in each person's zone on the send day. The send day is the day of the scheduled time in the account's time zone (or today when you send now). Sending starts when the first zone on Earth reaches that hour (UTC+14), so a whole day of zones takes about 26 hours. The campaign shows `sending` until the last zone is done. When you schedule it, the campaign's `send_at` becomes that first start time and `send_options.local_date` records the day.

Half-hour and quarter-hour zones (India, Nepal, Newfoundland, Adelaide) get their own release. On a daylight-saving change, a time the clock skips moves to the first time after it, and a time the clock passes twice uses the first.

### Best time for each person

DumbMail looks at each person's own opens and clicks from the last 180 days, by hour of the week in their zone, with recent ones counting more (half the weight every 30 days). It picks their best whole hour inside the window, within `within_hours` of the send time. People with fewer than 3 opens or clicks get the account's best hour (from everyone's engagement in the last 90 days, read in the account's zone and applied as each person's local hour); with too little account history, 10:00.

This is computed from past opens and clicks, so it's only as good as that history. Apple Mail's privacy protection opens many emails automatically at times the person didn't choose, which makes opens noisy. When someone has at least 3 clicks, only clicks count; otherwise clicks count twice as much as opens.

### Gradual send

The audience is split into `spread_hours` equal batches, one an hour, starting at the send time. Use it to warm up a new domain or to spread out replies to your support team.

### Smart sending

With `smart_sending` on, each recipient is checked when their message is about to go: if they got an email (for an email campaign) or a text (for an SMS campaign) from any campaign or flow in the last `smart_sending_hours`, they're skipped and recorded as skipped with the reason `smart sending`. Transactional email and test sends don't count. Flows have their own setting (`settings.smart_sending_hours` in the flow definition) using the same rule.

### What stays the same

- The audience is fixed when sending starts. Each person gets at most one message per campaign: release times are planned once, and a retried or restarted send never plans or sends anyone twice.
- A/B tests: the test group follows the same timing rule. The rest of the audience is planned when the winner is picked, by the same rule from that moment (for `local_time`, the next time it's that hour in their zone, or right away with `late: immediately`).
- Cancelling stops everything not yet sent, including zones still waiting.
- The warm-up limit, the automatic pause for bounces and complaints, billing, unsubscribes and suppressions apply as always.

## Estimates and progress

- `POST /api/sendtime/preview` with `{campaign_id}` (or `{channel, audience}`), optional `send_options` and `send_at`: how many people get it when, in 15-minute buckets with each bucket's main zones, and how many smart sending would skip right now. It samples up to 2,000 recipients and scales to the audience, so counts are estimates. MCP: `preview_send_plan`. `send_campaign`'s dry run includes the same as `send_plan`.
- `GET /api/campaigns/{id}/send-progress`: for each release time, its zones and how many messages are waiting, sent, skipped by smart sending, skipped for other reasons and failed; totals, the next release and skip reasons. The campaign report (`GET /api/campaigns/{id}/report`) includes it as `send_progress`. MCP: `get_send_progress`.

## SMS quiet hours

Texts from campaigns and flows only go out between 9:00 and 20:00 in the recipient's own time zone (found as above, falling back to the account's). A text outside those hours waits until the window opens; the rest of the batch goes on. Test sends aren't held.

A `timezone` property can come from a visitor's browser, so it never makes quiet hours looser than the phone number suggests: when the phone number points to a different zone, a text waits until it's within the hours in both zones. If the two windows never overlap, the phone number's zone decides.

You can make the window narrower, never wider than 8:00 to 21:00, and it must be at least an hour long:

```sh
curl -X PATCH https://dumbmail.ai/api/sendtime/settings \
  -H "Authorization: Bearer $DUMBMAIL_API_KEY" -H 'Content-Type: application/json' \
  -d '{"sms_window_start": "10:00", "sms_window_end": "19:00"}'
```

`GET /api/sendtime/settings` returns the window. MCP: `get_send_settings`, `update_send_settings`. A local-time SMS campaign must pick an hour inside the window, and best-time SMS sends stay inside it.
