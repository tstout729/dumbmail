# Flow tools

Extra flow steps and tools for people coming from Klaviyo: delays that wait until a time of day, a
weekday or a date; trigger splits on the event that started the flow; a notification step that
emails your team; adding people who already qualify (back-populate); and per-step results with the
list of people waiting at each step.

Everything here works the same in the dashboard, the REST API and the MCP server.

## Delays until a time, a weekday or a date

A `delay` step can take these extra fields. A delay with only `amount` and `unit` works exactly as
before.

| Field | What it does |
| --- | --- |
| `at` | A local time of day, `"HH:MM"`. "Wait 2 days, at 10:00": if 10:00 has already passed on the day the wait ends, it's 10:00 the next day. |
| `weekdays` | Allowed days, e.g. `["tue", "wed", "thu"]`. If the wait ends on another day it moves to the next allowed day (at `at` if set, else the same time of day). |
| `until` | Wait until a date instead of for a set time (see below). `amount` is ignored; send `0`. |
| `recompute_zone` | `true`: when the wait ends, work the time out again in the person's current time zone. Default `false`: keep the moment worked out when they reached the step. |

Times are in the person's time zone when DumbMail knows it (their `timezone` property, or one
worked out from their region, zip, country or phone number; see `dumbmail://docs/sendtime`), else
the account's.

Daylight saving: a local time that doesn't exist that day (02:30 on a spring-forward night) moves to
the first minute that does (03:00). A time that happens twice (01:30 on a fall-back night) is the
first one.

### `until`

```json
{"id": "d1", "type": "delay", "amount": 0, "unit": "days", "next": "e1",
 "until": {"field": "event.check_in", "offset_amount": 2, "offset_unit": "days", "offset_direction": "before", "if_past": "exit"},
 "at": "10:00"}
```

- `field`: `event.<property>` reads the triggering event's properties (`event.booking.check_in`
  for nested values); `profile.<property>` reads a profile property.
- The value can be a date (`2026-12-20`, the start of that day in the person's time zone), a
  date-time with or without a zone, or Unix seconds. Dates before 1900 or after 2199 count as
  unreadable.
- `offset_amount` (0 or more, up to a year), `offset_unit` (`minutes`, `hours`, `days`) and
  `offset_direction` (`before` or `after`, default `after`).
- `if_past`: what happens when the time has already passed (or the date is missing or unreadable)
  when the person reaches the step: `exit` takes them out of the flow, `continue` goes on to the
  next step at once. Default: `exit` for `before` offsets, `continue` for `after`.

If the flow is turned on and off, or a job wakes early, a person keeps waiting until the moment that
was worked out. The exit condition, unsubscribes and suppressions are checked when they wake, as at
every step.

## Trigger splits

A `condition` step can also test the event that started the run, with `event_condition`:

```json
{"id": "c1", "type": "condition", "condition": {"match": "all", "conditions": []},
 "event_condition": {"match": "all", "filters": [
   {"field": "properties.nights", "op": "gte", "value": 3},
   {"field": "from_browser", "op": "eq", "value": false}]},
 "yes": "long_stay", "no": "short_stay"}
```

- Fields: `properties.<name>` (dots for nested values), `value`, `metric` and `from_browser`.
- Operators: the same as trigger filters (`eq`, `neq`, `contains`, `gt`, `between`, `in`, `exists` ...),
  except the date ones. At most 20 filters.
- People go down `yes` only when the event matches and the profile matches `condition` (an empty
  profile condition matches everyone).
- Runs that no event started (list, segment and date triggers) have no event: filters that need a
  value don't match.

**Events from your website can't be trusted.** Anyone can send events with the public site key.
`from_browser` is `true` for those (and when it's unknown). Add `from_browser equals false` to split
only on events your server or an integration sent. The editor shows a note when a split or an
`until` delay reads an event your website can send. Message, consent and alert events
(`Received Email`, `Back in Stock`, `Price Dropped` ...) never come from a browser.

## Notification step

```json
{"id": "n1", "type": "notify", "name": "Tell the team",
 "to": ["owner@example.com", "manager@property.example"], "to_property": "manager_email",
 "subject": "New booking: {{ person.email }}",
 "body": "{{ person.first_name }} booked.\n\nCheck-in: {{ event.properties.check_in }}",
 "allow_browser_events": false, "next": null}
```

- Up to 5 recipients: fixed addresses in `to`, plus the address in the profile property
  `to_property`.
- **Who gets it:** your team members' addresses, and outside addresses that confirmed. The first time
  an outside address is saved in a flow it gets an email with a link to confirm; it gets nothing until
  someone opens the link and presses the button. The same rule applies to addresses read from a
  profile property. Each address gets at most one confirmation email an hour, and an account sends at
  most 50 a day. Addresses whose mail bounced or drew a complaint get none.
- `subject` (up to 200 characters) and `body` (plain text, up to 5,000) can use the person's and the
  event's fields, like emails. Coupon codes aren't allowed.
- It comes from your account's sender, has no unsubscribe link, and ends with "You get this because
  <account> set up a notification in a DumbMail flow". It isn't marketing and doesn't count for smart
  sending.
- Billed like any email on the hosted service, held to the sending rate and the account's pause.
- **Daily cap:** a flow sends at most `settings.notify_daily_cap` notifications a day (default 200,
  1-2,000), counted per local day of the account.
- Runs started by an event from your website get no notification unless `allow_browser_events` is
  `true`.
- Sent once: each run, step and address is recorded before it goes out, so a retried step never
  sends it twice. One cut off mid-send is marked `unknown` and not sent again.

REST: `GET /api/flows-plus/notify-addresses?emails=a@x.com,b@y.com` (outside addresses, and the
status of the ones asked: `team`, `confirmed`, `pending`, `removed`, `new`),
`POST /api/flows-plus/notify-addresses {email}` (send the confirmation link again),
`POST /api/flows-plus/notify-addresses/remove {email}`. MCP: `list_notify_addresses`,
`add_notify_address`, `remove_notify_address`.

## Add people who already qualify (back-populate)

List-, segment- and date-triggered flows start only for people who join or whose date comes after
the flow is turned on. Back-populate adds the people who already qualify.

- `POST /api/flows/{id}/backfill` without `confirm` (or MCP `backfill_flow`) counts them and changes
  nothing. With `"confirm": true` it starts adding them in the background. `limit` lowers the cap.
- The flow must be live. One back-populate per flow at a time, three per account.
- Capped per run at 50,000 people (`FLOW_BACKFILL_MAX` on self-hosted servers).
- Follows the flow's re-entry rules, profile filter and exit condition, checked for each person as
  they're added. Consent, suppressions and quiet hours are checked when each message goes out.
- **List flows:** everyone on the list now. **Segment flows:** everyone who matches the segment's
  rules now.
- **Date flows:** people whose trigger day (their date plus `offset_days`) already passed but who
  still have something ahead in the flow. They're placed where the flow would be for them today:
  delays already over are skipped, a delay still running is joined with the time it has left, and
  steps whose time has passed (an email that would have gone out yesterday) are skipped. Planning
  stops at the first split or `until` delay; they start there. People with nothing left aren't
  added. People whose trigger day is today or later are started by the daily date check as usual.
- **Metric flows can't be back-populated:** past events don't start flows, only new ones do.
- The set of people is fixed when it starts. Each person is marked done in the same transaction that
  starts their run, so if the job stops halfway it picks up where it left off and adds nobody twice.
- `POST /api/flows/{id}/backfills/{backfill_id}/cancel` (MCP `cancel_flow_backfill`) stops it.
  People already added stay in the flow. `GET /api/flows/{id}/backfills` (MCP `list_flow_backfills`)
  shows recent ones with how many were added and skipped.
- Starting and cancelling are recorded in the audit log.

## Per-step results and who is waiting

`GET /api/flows/{id}/results?days=30` (or `start=YYYY-MM-DD&end=YYYY-MM-DD` in the account's time
zone; MCP `get_flow_results`) returns, for every step:

- `reached`: runs that started in the range and passed through the step.
- `waiting`: people at the step right now.
- `messages` (email and SMS steps): recipients, sent, delivered, opened, clicked, bounced,
  unsubscribed, skipped (with the top three reasons), failed and open and click rates for messages
  created in the range, and `orders` and `revenue` the account's attribution settings credit to the
  step for conversions in the range.
- `notifications` (notification steps): sent, skipped and failed.

It is one query per flow and runs as a report (one at a time per account).

`GET /api/flows/{id}/steps/{node_id}/waiting?limit=50&after=<run_id>` (MCP `list_flow_waiting`)
lists the people at a step, oldest run first. Pass `next_after` back as `after` for the next page.

`POST /api/flows/{id}/runs/remove {"run_ids": [...], "confirm": true}` (MCP `remove_from_flow`)
takes up to 500 people out of a flow: they get nothing more from it. Without `confirm` it only
counts. Recorded in the audit log.

## Editing a flow while people wait

- A step that's deleted while people wait at it: they leave the flow when they reach it.
- A plain delay that's changed: people already waiting keep the time they had.
- A delay with `at`, `weekdays` or `until`: people keep the moment worked out when they arrived,
  unless the step has `recompute_zone` (then only their time zone is looked at again).

## Permissions

| What | Permission |
| --- | --- |
| Per-step results, back-populate history | Messaging read |
| The waiting list | Profiles read |
| Back-populate, cancelling it, taking people out of a flow | Turning flows live (`flows.live`) |
| Editing steps, notification addresses | Flow edit |
