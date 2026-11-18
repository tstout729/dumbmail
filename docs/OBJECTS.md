# Custom objects

Some things aren't a person or a moment: a reservation, a subscription, an appointment, a pet, a
vehicle, a membership. They have their own fields and dates, they change, and one person can have
several. Custom objects keep them in DumbMail, linked to the people they belong to, so flows can run
off their changes and dates and segments can ask about them.

## Object types

A type has a key (`reservation`), a name and plural (`Reservation`, `Reservations`), its fields and
an optional title field (shown as each object's title). Field types:

| Type | Stored as | Example |
| --- | --- | --- |
| `text` | text, up to 2,000 characters | `"Ocean view"` |
| `number` | number | `4` |
| `money` | number in the field's `currency` (major units) | `1240.5` |
| `boolean` | true or false | `true` |
| `date` | `YYYY-MM-DD`, a calendar day | `"2026-12-01"` |
| `datetime` | a moment, with its offset; stored in UTC | `"2026-12-01T16:00:00-08:00"` |
| `enum` | one of the field's `options` | `"confirmed"` |
| `list` | a list of text (up to 100 items) | `["wifi", "pets"]` |

Mark a field `sensitive` if it holds something private, such as a door code. Sensitive fields never
appear in lists or in emails and texts (they're simply absent from `object.*`), and in an object's own
view only people who can read profiles see them. Rules (segments, trigger filters, condition splits)
can't test them. Integration test runs and dry runs show them as `(hidden)`, and never say whether a
private value would change. Sending access details such as door codes needs a dedicated, separately
authorised delivery that doesn't exist yet, so keep them out of messages (and out of objects if you
can). A sensitive field taken off a type stays sensitive: its stored values and history are
deleted in the background, and a later field with the same name is sensitive again.

A type is also made the first time an object of it is upserted, with each field's type guessed from
its first value (`2026-12-01` is a date, `true` a boolean, `4` a number, other text is text). A new
key on a later upsert adds a field the same way. Fix guesses, add enum options, labels and the
title field with `upsert_object_type` (or Objects → the type → Fields in the dashboard).

Limits: 50 types per account, 100 fields per type, 32 KB of fields per object, 20 linked people per
object.

```json
{"key": "reservation", "name": "Reservation", "plural": "Reservations", "title_field": "confirmation_code",
 "fields": [
   {"key": "status", "type": "enum", "options": ["confirmed", "cancelled", "inquiry"]},
   {"key": "confirmation_code", "type": "text"},
   {"key": "check_in", "type": "date", "label": "Check-in"},
   {"key": "check_out", "type": "date", "label": "Check-out"},
   {"key": "total", "type": "money", "currency": "USD"},
   {"key": "door_code", "type": "text", "sensitive": true}
 ]}
```

## Objects

Upsert by type and `external_id` (the object's id in your system):

```json
{"type": "reservation", "external_id": "R-1042",
 "fields": {"status": "confirmed", "check_in": "2026-12-01", "check_out": "2026-12-05", "total": 1240.5},
 "profiles": [{"email": "ana@example.com", "relationship": "guest"}],
 "as_of": "2026-11-17T10:00:00Z"}
```

- `fields` merge into what's stored; `null` removes a field. Values are checked against the field's
  type and stored in one form (dates as `YYYY-MM-DD`, date-times in UTC).
- `profiles`: people it belongs to, by `id` or by `email`, `phone_number` or `external_id`, with an
  optional `relationship` (`guest`, `owner` ...). They're added to the people already linked. An
  existing profile is found, never changed. Someone new is created with only those identifiers and
  has never subscribed to anything (objects never change anyone's consent).
- `as_of`: the change's time (or version number) in your system. An upsert older than the newest
  one written is ignored, so a late message never undoes a newer one.
- Every change is recorded: what changed, from what to what, when and by whom (`api`, `mcp` or
  `integration:<id>`). Writing the same values again changes nothing and records nothing. History
  keeps the newest 100 changes per object, for up to a year.
- A status is just a field (an enum like `status: confirmed | cancelled`). Deleting an object
  removes it with its history; people in flows for it leave those flows.

Objects are written only by the API, MCP, bulk upserts and integrations. There's no browser or
public way to write them.

## Flows off objects

Two triggers. Both start one run for each person linked to the object (`relationship` narrows it,
e.g. only `guest`), and the run remembers its object.

**Object created or updated**

```json
{"type": "object", "object_type": "reservation", "on": "updated", "field": "status", "to": "cancelled",
 "filters": [{"field": "check_in", "op": "in_future"}]}
```

- `on`: `created`, `updated` or `any` (default).
- `field`: only when this field changed; `to`: only when it changed to this value.
- `filters`: on the object's fields after the change.
- Re-entry (`never`, `always`, `after_days`) counts runs for the same object only: a guest with two
  reservations goes through the flow for each.

**N days or hours before or after a date field**

```json
{"type": "object_date", "object_type": "reservation", "field": "check_in", "offset_days": -2, "hour": 10,
 "relationship": "guest", "filters": [{"field": "status", "op": "eq", "value": "confirmed"}]}
```

- `offset_days` (−365 to 365) with `hour` (0-23, default 9) in the account's time zone, or
  `offset_hours` (−8760 to 8760) on its own for a moment N hours before or after (a plain date's
  moment is its local midnight). Day boundaries and daylight saving follow the account's time zone,
  and pending sends move if the account's time zone changes. On the night clocks go forward, an
  hour that doesn't exist (02:00 in Los Angeles on 8 March 2026) fires an hour later (03:00); on the
  night they go back, the repeated hour (01:00 on 1 November 2026) fires once, at the second one
  (01:00 standard time).
- Each object starts the flow once for each moment it is due: a guest with two reservations gets
  two runs. If the date moves, the old moment is dropped and the new one scheduled; a moment that
  already sent never sends again.
- Due objects are checked with the other date-based flows, about every half hour. A send found
  more than a day late (the scheduler was down) is recorded as missed instead of going out late.
  When a flow goes live, existing objects are scheduled in batches.
- `filters` are checked when it's due: a cancelled reservation doesn't start it.
- The flow's report (`object_dates`) counts fires waiting, started, filtered and missed.

**While a run is in the flow**, before each step it ends (the person leaves) when the object was
deleted, the person was unlinked from it, its date moved (date triggers), or it no longer matches
the trigger's filters (its status became `cancelled`, say).

**In messages**, the object's current fields (never its sensitive ones) are `object.*`: `{{ object.listing_name }}`,
`{{ object.check_in|date }}` ("December 1, 2026"), `{{ object.check_in|date('%a %b %-d') }}`. The
`date` filter shows a date-time in the account's time zone. `object.external_id` and `object.type`
are there too.

**Condition splits** can test the object: add `object_filters` (same rules as segment field rules)
to a condition step. Both its segment rules and its object filters must hold for "yes"; in a run
without an object, object filters are never true.

```json
{"id": "s1", "type": "condition", "condition": {"conditions": []},
 "object_filters": [{"field": "nights", "op": "gte", "value": 7}], "yes": "long", "no": "short"}
```

**Who gets messages.** Object flows use the normal send path: unsubscribes and suppressions always
win, texts need SMS consent, and people held back from marketing (added by an integration without an
opt-in) get email only from flows marked `service_messages: true`.

## Segments

```json
{"type": "object", "object_type": "reservation", "op": "at_least", "count": 1, "relationship": "guest",
 "where": [{"field": "status", "op": "eq", "value": "confirmed"}, {"field": "check_in", "op": "within_next_days", "value": 14}]}
```

- `op`: `at_least` (default, with `count`, default 1), `at_most`, `exactly` or `zero` ("doesn't
  have"). For example "no active reservation": `op: zero` with `check_out` `in_future`.
- `where`: field rules on the object's fields, with the usual operators (`eq`, `neq`, `contains`,
  `in`, `gt`, `between`, `exists` ...) and three for dates relative to today:
  `within_next_days` (today or the next N days), `in_future` (today or later) and `in_past` (before
  today). A plain date is compared by calendar day in the account's time zone, a date-time with
  now. `within_last_days` and `more_than_days_ago` work too.

## Integrations

Translation scripts can return two more actions (see dumbmail://docs/integrations):

```js
{type: "upsert_object", object_type: "reservation", external_id: "R-1042",
 fields: {status: "confirmed", check_in: "2026-12-01"},
 profiles: [{email: "ana@example.com", relationship: "guest"}], as_of: "2026-11-17T10:00:00Z"}
{type: "delete_object", object_type: "reservation", external_id: "R-1042"}
```

`profiles` find people the way other actions do (email, phone_number, external_id, or property +
value with `if_missing`); someone new is held back from marketing unless the integration's contacts
opted in. The integration's redact list is applied to the fields too, so `**.doorCode` keeps a door
code out of objects. An object without an `external_id` gets one from the delivery. Retries and
replays apply the stored action once, like the other actions; dry runs and tests show what would
change.

The Guesty recipe keeps each reservation as a `reservation` object and suggests a pre-arrival flow
off its check-in date.

## API

| Method | Path | Permission |
| --- | --- | --- |
| GET | `/api/object-types` | audience read |
| POST | `/api/object-types` (create or change by key) | settings write |
| GET, PATCH | `/api/object-types/{key}` | audience read, settings write |
| DELETE | `/api/object-types/{key}?confirm=true` | settings write |
| GET | `/api/object-types/{key}/objects?q=&limit=&offset=` | audience read |
| POST | `/api/objects` (one upsert) | audience write |
| POST | `/api/objects/bulk` `{"objects": [...]}` (up to 1,000, 8 MB; returns 202 and an id) | audience write |
| GET | `/api/objects/bulk/{id}` | audience read |
| GET | `/api/objects/{id}` (with history) | audience read; linked people and sensitive fields need profile read |
| DELETE | `/api/objects/{id}` | audience write |
| POST | `/api/objects/delete` `{"ids": [...]}` or `{"type", "external_ids": [...]}`, `"confirm": true` | audience write |
| GET | `/api/profiles/{id}/objects` | profile read |

A bulk upsert is written by a background job, 100 at a time; an account can have 3 waiting or
running. Its status has counts (created, updated, unchanged, failed) and the first 50 errors.

Deleting a type is refused while a flow or segment uses it. Deleting a type and deleting objects
are recorded in the audit log (ids, type keys and counts, never field values).

MCP tools: `list_object_types`, `upsert_object_type`, `delete_object_type` (confirm), `list_objects`,
`get_object`, `upsert_objects` (up to 50 written at once, more queued as a bulk upsert),
`get_object_bulk_upsert`, `delete_object` (confirm).

Object changes aren't sent to webhooks or data exports yet.
