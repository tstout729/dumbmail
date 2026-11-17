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

Set `send_options` on a campaign (`POST /api/campaigns`, `PATCH /api/campaigns/{id}`, or with the send itself: `POST /api/campaigns/{id}/send` with `{"send_options": ...}`, `POST /api/campaigns/{id}/schedule` with `{"send_at", "send_options"}`, or MCP `create_campaign`, `update_campaign`, `send_campaign`). Options given with a send are checked and saved on the campaign with the send itself, so they're kept only if it goes out; unknown fields are refused. `null` or leaving it out sends to everyone at once with no smart sending, as before.

| Field | Used with | Meaning |
| --- | --- | --- |
| `timing` | all | `all_at_once` (default), `local_time`, `best_time` or `gradual` |
| `local_time` | `local_time` | The hour in each person's zone, `HH:MM` (default `10:00`) |
| `late` | `local_time` | For people whose hour had already passed more than 15 minutes before the send was asked for: `next_day` (default) gets it at that hour tomorrow, `immediately` gets it right away. Anyone whose hour passed less than 15 minutes before gets it right away either way |
| `window_start`, `window_end` | `best_time` | The local hours it may go out in (default `08:00` to `20:00`, at least an hour) |
| `within_hours` | `best_time` | How far ahead of the send time to look (6 to 72, default 24). Someone whose window doesn't open within it gets it when the window next opens, after it |
| `spread_hours` | `gradual` | Hours to spread the send over, one equal batch an hour (2 to 72) |
| `smart_sending` | all | `true` skips people who got a message on this channel recently |
| `smart_sending_hours` | all | How recently (1 to 168 hours, default 16) |

Example:

```json
{"send_options": {"timing": "local_time", "local_time": "10:00", "late": "next_day", "smart_sending": true}}
```

### Each person's local time

The campaign goes out at `local_time` in each person's zone on the send day. The send day is the day of the scheduled time in the account's time zone (or today when you send now). Sending starts when the first zone on Earth reaches that hour (UTC+14), so a whole day of zones takes about 26 hours. The campaign shows `sending` until the last zone is done. When you schedule it, the campaign's `send_at` becomes that first start time and `send_options.local_date` records the day.

Whether a zone's hour "already passed" is judged against when the send was asked for (the campaign's `send_at`: the click on Send, or the scheduled start), with 15 minutes' grace. So the first zone of a scheduled send, or New York when you click Send at 10:05 there, gets it right away rather than tomorrow.

Half-hour and quarter-hour zones (India, Nepal, Newfoundland, Adelaide) get their own release. On a daylight-saving change, a time the clock skips moves to the first minute after the gap (02:07 on a 02:00 to 03:00 gap goes at 03:00), and a time the clock passes twice uses the first.

### Best time for each person

DumbMail looks at each person's own opens and clicks from the last 180 days, by hour of the week in their zone, with recent ones counting more (half the weight every 30 days). It picks their best whole hour inside the window, within `within_hours` of the send time. With a short `within_hours`, someone's window may not open before it ends (21:00 for them with a 6-hour look-ahead and an 08:00 to 20:00 window): they get it when their window next opens, which is after the look-ahead. People with fewer than 3 opens or clicks get the account's best hour (from everyone's engagement in the last 90 days, read in the account's zone and applied as each person's local hour); with too little account history, 10:00.

This is computed from past opens and clicks, so it's only as good as that history. Apple Mail's privacy protection opens many emails automatically at times the person didn't choose, which makes opens noisy. When someone has at least 3 clicks, only clicks count; otherwise clicks count twice as much as opens.

### Gradual send

The audience is split into `spread_hours` equal batches, one an hour, starting at the send time. Use it to warm up a new domain or to spread out replies to your support team.

### Smart sending

With `smart_sending` on, each recipient is checked when their message is about to go: if they got an email (for an email campaign) or a text (for an SMS campaign) from any campaign or flow in the last `smart_sending_hours`, they're skipped and recorded as skipped with the reason `smart sending`. Transactional email and test sends don't count. Flows have their own setting (`settings.smart_sending_hours` in the flow definition) using the same rule.

### What stays the same

- The audience is fixed when sending starts: people who join a list or segment later don't get it, even if the send is retried. Unsubscribes, bounces and complaints are still checked for everyone just before their message goes.
- Each person gets at most one message per campaign. Release times are computed from when the send was asked for, not from when the planning step happens to run, so a planning step that's interrupted and run again gives everyone the same time, and a gradual send keeps its equal batches.
- A/B tests: the test group follows the same timing rule. The rest of the audience is fixed when the test starts (so someone who leaves the list during the test still gets the winner, if they can still be emailed), and planned from when the winner is picked (stored, so a restart gives the same times), by the same rule (for `local_time`, the next time it's that hour in their zone, or right away with `late: immediately`). An A/B test that was already running when this was released has no stored rest of the audience: its winner goes to the audience as it is when the winner is picked.
- Cancelling stops everything not yet sent, including zones still waiting. A message a worker had already picked up when the cancel landed is either already on its way or skipped; it's never left waiting. Each message is checked one last time right before it's handed to the provider (after any wait for a sending slot): an unsubscribe, a suppression, a cancel or a pause that landed meanwhile stops it.
- Editing a cancelled or failed campaign returns it to draft and forgets its last attempt: the next send is planned afresh for the audience it has then. People who already got it (or whose message may have gone out) never get a second copy.
- The warm-up limit, the automatic pause for bounces and complaints, billing, unsubscribes and suppressions apply as always.

## Estimates and progress

- `POST /api/sendtime/preview` with `{campaign_id}` (or `{channel, audience}`), optional `send_options` and `send_at`: how many people get it when, in 15-minute buckets with each bucket's main zones, and how many smart sending would skip right now. It samples up to 2,000 recipients and scales to the audience, so counts are estimates. MCP: `preview_send_plan`. `send_campaign`'s dry run includes the same as `send_plan`.
- `GET /api/campaigns/{id}/send-progress`: for each release time, its zones and how many messages are waiting, sent, skipped by smart sending, skipped for other reasons and failed; totals, the next release and skip reasons. The campaign report (`GET /api/campaigns/{id}/report`) includes it as `send_progress`. MCP: `get_send_progress`.

## SMS quiet hours

Texts from campaigns and flows only go out between 9:00 and 20:00 in the recipient's own time zone. A text outside those hours waits until the window opens; the rest of the batch goes on. Hours are checked again just before each text is handed to the carrier (the whole decision, including the setting below), so one waiting for its turn never goes out after the window closes. Test sends aren't held.

Whose clock counts: the phone number's own zone (from its country, or its US or Canadian area code) is the authority, because carriers and regulators go by it and nobody can set it from a browser. The profile's zone (found as above) can only narrow the hours: when the two differ, a text waits until it's within the hours in both. When there's no zone from the phone number (a country with several zones and no known area code), the account's zone takes its place: a text waits until it's within the hours in both the account's zone and the profile's, and if those never share texting hours, the account's zone decides (and the message notes it).

When the two zones never share texting hours (a New York number on a profile that says Manila), the account decides, in Settings → Send times or with `sms_zone_conflict`:

- `phone` (default): text them during the phone number's hours.
- `skip`: don't text them. The message is skipped with the reason `time zones disagree`.

Either way the message records it (`send_note`), shown on the person's profile under Messages received and, when sent, on the "Received SMS" event as `time_zone_note`.

You can make the window narrower, never wider than 8:00 to 21:00, and it must be at least an hour long:

```sh
curl -X PATCH https://dumbmail.ai/api/sendtime/settings \
  -H "Authorization: Bearer $DUMBMAIL_API_KEY" -H 'Content-Type: application/json' \
  -d '{"sms_window_start": "10:00", "sms_window_end": "19:00", "sms_zone_conflict": "phone"}'
```

`GET /api/sendtime/settings` returns the window and `sms_zone_conflict`. MCP: `get_send_settings`, `update_send_settings`. A local-time SMS campaign must pick an hour inside the window, and best-time SMS sends stay inside it.
