# Reports

Custom and ready-made reports, attribution settings, cohorts, funnels, deliverability by inbox
provider, benchmarks and CSV exports. Everything here is in the dashboard (Analytics), the REST API
(all routes under `/api/reports`, signed in or with `Authorization: Bearer <API key>`) and the MCP
server (tools listed at the end).

Dates and day boundaries use the account's time zone (Settings → Account, default
`America/Los_Angeles`).

## Attribution settings

Revenue is credited to a message by one rule, used by every report: the analytics overview, campaign
and flow reports, custom reports, cohorts and funnels. Change it and every report follows, including
past dates.

| Field | Default | Meaning |
| --- | --- | --- |
| `email_open_window_hours` | 120 (5 days) | An email opened this long before a conversion can get credit. |
| `email_click_window_hours` | 120 (5 days) | An email clicked this long before a conversion can get credit. |
| `sms_click_window_hours` | 24 | A text clicked this long before a conversion can get credit. |
| `count_opens` | `true` | `false` is "clicks only". Apple Mail opens many emails by itself, so opens can over-credit email. |
| `model` | `last_touch` | `last_touch` credits the message touched most recently; `first_touch` the one touched earliest. |
| `conversion_metrics` | `["Placed Order"]` | Events of these metrics (1-5) are conversions. Their `value` is the revenue. |

Windows are 1-720 hours (30 days). A message counts only for interactions before the conversion.
One conversion metric is also saved as the account's `conversion_metric` (which `update_account`
changes too); two or more are kept here and take precedence.

| Method | Path | Body | Returns |
| --- | --- | --- | --- |
| GET | `/api/reports/attribution` | | The settings, plus `description` (plain words) and `is_default`. |
| PATCH | `/api/reports/attribution` | Any of the fields above | The saved settings. |
| POST | `/api/reports/attribution/preview` | `{settings: {...fields}, days: 30}` | `{days, current: {settings, totals}, proposed: {settings, totals}}`. Nothing is saved. `totals` has `conversions, revenue, attributed_conversions, attributed_revenue, campaign_revenue, flow_revenue, email_revenue, sms_revenue` for the last `days` days. |

## Report spec

A report is a JSON spec. Only the names listed below are accepted; every value is bound into the
query, never written into it. Each run stops after 20 seconds (2 minutes for scheduled emails) and
returns at most 1,000 rows.

```json
{
  "source": "campaigns",
  "measures": ["sent", "open_rate", "click_rate", "revenue"],
  "group_by": ["campaign"],
  "filters": [{"field": "channel", "values": ["email"]}],
  "range": {"days": 90},
  "compare": true,
  "sort": {"by": "revenue", "desc": true},
  "limit": 50
}
```

| Key | Meaning |
| --- | --- |
| `source` | What to count (table below). Required. |
| `measures` | Up to 20. Left out: the source's usual ones. |
| `group_by` | Up to 2. Left out: one row of totals. Only one of day, week or month. |
| `filters` | Up to 10: `{field, values (1-100), exclude?}`. Property filters also take `key`. |
| `range` | `{"days": N}` is the last N x 24 hours up to now (the same window the overview uses), 1-731. `{"start": "2026-10-01", "end": "2026-10-31"}` are dates in the account's time zone, both included (end defaults to today). Default: 30 days. |
| `compare` | Also total the same length of time just before the range (`previous`). |
| `metric` | For `source: metric`: the metric's name. |
| `property` | For `group_by: property`: the event property's name. |
| `sort` | `{by: <measure> or "group", desc}`. Default: time order for time groups, else the first measure, largest first. |
| `limit` | 1-1000 rows. Default 1000 with a time group, else 100. |

### Sources

| Source | Counts | group_by | filters |
| --- | --- | --- | --- |
| `campaigns` | Campaign messages sent in the range, and conversions in the range credited to them | day, week, month, hour, weekday, channel, campaign, country, list, segment | channel, campaign, country, list, segment |
| `flows` | Flow messages sent in the range, and conversions credited to them | day, week, month, hour, weekday, channel, flow, message, country | channel, flow, country |
| `flow_messages` | The same, meant per flow step (`message`) | message, flow, channel, day, week, month | channel, flow, country |
| `messages` | Every message | day, week, month, hour, weekday, channel, campaign, flow, message, source, country, list, segment | channel, campaign, flow, country, list, segment |
| `revenue` | Every conversion in the range, credited or not | day, week, month, hour, weekday, channel, source, campaign, flow, message, country | channel, source, campaign, flow, country |
| `forms` | Sign-up form views and sign-ups | day, week, month, hour, weekday, form | form |
| `list_growth` | People joining lists, unsubscribing, hard bouncing, marking as spam | day, week, month, list | list |
| `metric` | Events of one metric (`metric`) | day, week, month, hour, weekday, country, property, campaign, flow | country, property |
| `customers` | People who converted, and how many had converted before | day, week, month | country |
| `engagement` | People you can email, by when they last opened or clicked (as of now; the range doesn't apply) | tier | list, segment |

Products aren't a source yet: DumbMail doesn't have a product catalog to report on.

### Measures

- **Message sources** (`campaigns`, `flows`, `flow_messages`, `messages`): `sent`, `delivered`,
  `recipients` (different people), `opens` and `clicks` (unique, one per message), `unsubscribes`,
  `bounces`, `hard_bounces`, `complaints`, `conversions`, `revenue`, `open_rate` (opens ÷ delivered
  emails), `click_rate` (clicks ÷ delivered), `click_to_open_rate`, `unsubscribe_rate` (÷ sent),
  `bounce_rate` and `complaint_rate` (÷ emails sent), `conversion_rate` (conversions ÷ recipients),
  `revenue_per_recipient`, `average_order_value`. Messages count by when they were sent; revenue
  counts by when the conversion happened, so a report's totals match the overview for the same days.
- **revenue**: `conversions`, `revenue`, `attributed_conversions`, `attributed_revenue`,
  `attributed_share`, `customers`, `average_order_value`.
- **forms**: `views`, `sign_ups`, `sign_up_rate`.
- **list_growth**: `new_profiles` (not per list), `joined`, `unsubscribed`, `bounced` (hard bounces),
  `complaints`, `net_change` (joined - unsubscribed - bounced). Per list, an unsubscribe or bounce
  counts for every list the person is on, plus the list they left on the preference page; across the
  account only full unsubscribes count.
- **metric**: `events`, `people`, `value`, `average_value`.
- **customers**: `customers`, `returning_customers` (had converted before), `repeat_purchase_rate`,
  `orders`, `revenue`, `revenue_per_customer`, `orders_per_customer`.
- **engagement**: `profiles`, `share`.

### Groups

`day`, `week` (starting Monday), `month`, `hour` (00-23) and `weekday` (1 = Monday) are in the
account's time zone. `channel` is email or SMS. `source` is campaign, flow or (revenue only) none.
`message` is a flow step (`<flow id>/<step id>`) or a campaign. `list` and `segment` on message
sources are the campaign's audience, so a campaign sent to two lists shows under both. `country` is
the profile's `country` property. Engagement `tier`s: `active_30`, `active_90` (31-90 days ago),
`active_180` (91-180), `at_risk` (last active 6-12 months ago), `unengaged` (emailed in the last
year, never opened or clicked), `not_emailed` (no email in the last year).

### Result

```json
{
  "spec": {...}, "timezone": "America/Los_Angeles",
  "range": {"start": "...", "end": "..."}, "previous_range": {...} | null,
  "columns": [{"key": "campaign", "label": "Campaign", "kind": "group"}, {"key": "revenue", "label": "Revenue", "kind": "currency"}],
  "rows": [{"keys": ["<id>"], "labels": ["Spring sale"], "values": {"revenue": 190.0}}],
  "totals": {"revenue": 190.0}, "previous": {"revenue": 120.0} | null,
  "truncated": false,
  "attribution": "Placed Order counts when ..."
}
```

Measure kinds are `count`, `rate` (a fraction: 0.25 is 25%), `currency` and `number`.

### Examples

Revenue by channel and source, this month against last month:

```json
{"source": "revenue", "group_by": ["source", "channel"], "range": {"start": "2026-11-01"}, "compare": true}
```

Send-time heatmap:

```json
{"source": "messages", "group_by": ["weekday", "hour"], "range": {"days": 90}, "measures": ["sent", "open_rate", "click_rate"]}
```

Product views by a property, Canada only:

```json
{"source": "metric", "metric": "Viewed Product", "group_by": ["property"], "property": "category",
 "filters": [{"field": "country", "values": ["CA"]}]}
```

### Routes

| Method | Path | Body | Returns |
| --- | --- | --- | --- |
| GET | `/api/reports/catalog` | | Every source with its measures, groups and filters. |
| POST | `/api/reports/run` | A spec | The result. |
| POST | `/api/reports/run.csv` | A spec | The result as CSV (rates in percent, with a Total row). |
| GET | `/api/reports` | | `{builtin: [...], saved: [...]}` |
| POST | `/api/reports` | `{name, description?, spec, schedule?, recipients?}` | The saved report (201). At most 200 per account. |
| GET | `/api/reports/{id}` | | A saved report (`<uuid>`) or a ready-made one (`builtin:<key>`). |
| GET | `/api/reports/{id}/run` | | `{report, result}` |
| PATCH | `/api/reports/{id}` | Any of the create fields; `"schedule": null` stops emails | The saved report. Ready-made reports can't be changed. |
| POST | `/api/reports/{id}/duplicate` | `{name?}` | A new saved copy (201). |
| DELETE | `/api/reports/{id}` | | 204 |
| GET | `/api/reports/members` | | Members who can get scheduled emails. |

### Ready-made reports

`builtin:campaign_performance`, `builtin:flow_steps`, `builtin:list_growth`,
`builtin:revenue_by_source`, `builtin:form_conversion`, `builtin:engagement_tiers`,
`builtin:repeat_purchase` and `builtin:send_heatmap`. Duplicate one to change it.

### Scheduled emails

`schedule` is `weekly` (Mondays) or `monthly` (the 1st), sent at 8 a.m. in the account's time zone
to 1-10 `recipients` (user ids of the account's members). Each email has the totals and top 10 rows,
and links to the full CSV, which is kept with the account's exports for 7 days and downloads only
while signed in. Emails come from the server's `SUPPORT_EMAIL`; without it the report shows
`last_error` instead. People who leave the account stop getting them.

## Cohorts

`GET /api/reports/cohorts?by=first_purchase&months=12` (`by`: `first_purchase` or `signup`; months
1-24; add `&format=csv` for a file). People are grouped by the month of their first conversion (or
when their profile was created). For each month since, `cells[k]` has `customers` (people who
bought again that month; for sign-up cohorts, who bought at all), `rate` (÷ cohort size), `revenue`,
and `cumulative_revenue_per_person`.

## Funnels

`POST /api/reports/funnel`

```json
{"steps": ["Viewed Product", "Added to Cart", "Started Checkout", "Placed Order"], "window_days": 7,
 "range": {"days": 30}, "split_by": {"type": "source"}}
```

People who did step 1 in the range count once, from their first step 1. Each later step must happen
in order, within `window_days` (1-90) of that first step. `split_by` is optional:
`{"type": "event_property", "key": "utm_source"}` (of the step 1 event),
`{"type": "profile_property", "key": "plan"}`, or `{"type": "source"}` (whether step 1 came from a
campaign or flow message, by the attribution settings). The 10 biggest splits are kept; the rest are
"Other". Each step returns `people`, `conversion_from_previous`, `conversion_from_start` and
`dropped`. A funnel's range can cover at most 365 days.

## Deliverability

`GET /api/reports/deliverability?days=30&interval=day` (days 1-365; interval `day` or `week`).
Email only. For each inbox provider (Gmail, Outlook and Hotmail, Yahoo and AOL, Apple iCloud, other)
and each sending domain: sent, delivered, opens, clicks, unsubscribes, bounces (hard and soft),
complaints and their rates, a `health` score (0-100, `good` 85+, `fair` 65+, else `poor`) with
plain-language `flags`, and a `series` over time. Providers come from the recipient's email domain;
custom domains are matched by their public MX records (Google Workspace, Microsoft 365, Yahoo,
iCloud), looked up in the background after you open the report and cached for 30 days. Sending
domains are recorded from now on; earlier email shows as "not recorded". `authentication` lists each
sending domain's DKIM, SPF and DMARC status from its last check in Settings → Domains.

Flags need 100 emails or more. They fire at: complaints 0.1% (and 0.3%, Gmail's limit), bounces 2%
(and 5%, where Amazon SES reviews senders), unsubscribes 0.5% and 1%, opens under 10% with 500+
delivered.

## Benchmarks

`GET /api/reports/benchmarks?industry=ecommerce` shows the account's email open, click,
unsubscribe, bounce and complaint rates over the last 30 days next to its last 90 days and published
figures: MailerLite's Email Marketing Benchmarks (published 2025-12-03; medians of 3.6 million
campaigns sent December 2024 to November 2025) for opens, clicks and unsubscribes by industry, with
the range across all 46 industries; Amazon SES's bounce guidance (under 2%) and Google's sender
guidelines for spam complaints (under 0.1%, never 0.3%). Nothing is computed from other DumbMail
accounts. The table lives in `src/features/reports/benchmarks.json`.

## Exports

| Method | Path | Body | Returns |
| --- | --- | --- | --- |
| POST | `/api/reports/exports` | See below | The export (202, `status: queued`). 409 while another export runs. |
| GET | `/api/reports/exports` | | Recent exports. |
| GET | `/api/reports/exports/{id}` | | `{id, kind, status, filename, row_count, size_bytes, error, download_url, expires_at, ...}` |
| GET | `/api/reports/exports/{id}/download` | | The CSV file (signed in or with an API key). |

Bodies:

- `{"kind": "profiles", "scope": "all" | "list" | "segment", "list_id"?, "segment_id"?, "columns"?, "properties"?}`.
  Columns: `id, email, phone_number, external_id, first_name, last_name, email_consent, sms_consent,
  email_suppressed_reason, created_at, updated_at, last_event_at` (default email, phone, names,
  consent, created). `properties`: up to 50 custom property names, one column each.
- `{"kind": "events", "metric": "Placed Order", "start"?, "end"?}`: dates in the account's time zone,
  default the last 30 days.
- `{"kind": "campaign_recipients", "campaign_id": "..."}`: each recipient's version, status, bounce
  type, sent/delivered/opened/clicked/unsubscribed times and the conversions and revenue credited to
  the campaign.
- `{"kind": "suppressions"}`: everyone unsubscribed from email or SMS, or suppressed after a hard
  bounce or complaint.

Exports run in the background, 5,000 rows at a time. One runs per account at a time. Files are kept
7 days and can be up to 100 MB. Times are in the account's time zone with their offset. Cells that
start with `=`, `+`, `-` or `@` get a leading `'` so spreadsheets don't run them as formulas.

## MCP tools

`get_report_catalog`, `run_report` (`{spec}`), `list_reports`, `get_report` (`{id}`: definition and
results), `save_report` (create, or update with `id`; setting a schedule needs `confirm: true`),
`duplicate_report`, `delete_report` (`confirm: true`), `list_report_recipients`, `cohort_report`,
`funnel_report`, `deliverability_report`, `benchmark_report`, `get_attribution_settings`,
`update_attribution_settings` (`preview: true` shows the change without saving), `start_export`,
`list_exports`, `get_export`.
