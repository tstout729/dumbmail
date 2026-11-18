# Link tagging, resends, tags and the calendar

Four campaign tools: UTM link tagging, resending to people who didn't open (or click), tags and
archiving for campaigns, flows and templates, and a calendar of campaigns.

## Link tagging (UTM)

When it's on, every web link in a campaign or flow email gets UTM parameters, so your analytics
tool can tell which email brought a visitor. Links in text messages get them too when they're
shortened (Settings → Texting, on by default; see the short links doc): a short link is the same
length whatever it points to, so tags cost nothing. A text link that keeps the long tracked form
carries its whole destination, so it isn't tagged (tags could cost another segment).

- Turn it on and set the values in Settings → Tracking (`GET`/`PUT /api/link-tags`, MCP
  `get_link_tagging` / `update_link_tagging`). Off by default.
- Values: `utm_source` (default `dumbmail`), `utm_medium` (default `{channel}`: `email` or `sms`),
  `utm_campaign` (default: the campaign or flow name, slugged, plus the first 8 characters of its
  id, like `black-friday-0190aaaa`), optional `utm_content` and `utm_term`, and up to 10 custom
  parameters (names: 1–40 lowercase letters, digits or underscores). Values are at most 200
  characters.
- Values may use these variables, and nothing else in braces: `{campaign_name}`, `{campaign_id}`,
  `{flow_name}`, `{flow_id}`, `{message_name}` (the campaign name, or the flow step's name),
  `{channel}`, `{send_date}` (YYYY-MM-DD in the account's time zone) and `{version}` (the A/B
  version letter). A variable with no value in this message (say `{flow_name}` in a campaign)
  is empty, and an empty parameter is left out.
- `skip_domains`: links to these domains and their subdomains are never tagged (at most 50).
- Each campaign and flow can use the defaults, turn tagging off, or set its own values:
  `PUT /api/campaigns/{id}/link-tags` or `/api/flows/{id}/link-tags` with
  `{"mode": "default" | "off" | "custom", "params": {...}}` (MCP `set_campaign_link_tags`,
  `set_flow_link_tags`). A campaign's can change only while the campaign can be edited.
- `GET /api/campaigns/{id}/link-tags` shows what its links will get, with the variables filled in
  (`preview`, like `utm_source=dumbmail&utm_medium=email&utm_campaign=...`).

What gets tagged: the destination of each link that click tracking wraps in an email's HTML, and of
each short link in a text. Only
`http` and `https` links. Never DumbMail's own links (unsubscribe, preferences, view online,
tracking, and older DumbMail link hosts that still work),
`mailto:` or `tel:` links, or a skipped domain. A parameter the link already has is kept, not
replaced. The rest of the link, its query string and its `#fragment` stay exactly as written. An
email's plain-text part isn't tagged.

When the tags are set: a campaign's are worked out once, when it starts sending, from the settings
at that moment, and every recipient gets the same ones (`{send_date}` is the day it started). A
flow's are worked out for each message as it's sent. A test send uses the tags the real send would
use today. Messages with tagging off are rendered exactly as before.

## Resend to people who didn't open

From a sent email campaign: `POST /api/campaigns/{id}/resend` with optional
`{"target": "no_open" | "no_click", "subject", "preheader", "name"}` (MCP `create_resend`). It
makes a **draft** campaign with the same content (the A/B winner's, if there was a test); nothing
is sent until you send it like any other campaign (`send_campaign`, which needs `confirm=true`).

Who gets it, worked out once when the resend starts sending and never again on a retry:
everyone the original went to (sent or delivered; not bounced, failed or skipped) who hasn't opened
(`no_open`; a click counts as an open) or clicked (`no_click`) the original or an earlier resend of
it by then, whose email address is still the one the original went to, and who is subscribed to
email now. Right before each email goes out, the address and subscription are checked again; anyone
whose address changed or who isn't subscribed any more is skipped. Lists and segments don't apply.
Unsubscribes, suppressions, smart sending, sunsetting and every other send-time check apply as usual.

A resend stays a resend: its audience and channel can't be changed (through the dashboard, the API
or MCP). Its subject, preview text, content and send options can.

Rules:

- Email only. The original must have finished sending (after an A/B test, the winner send) at least
  4 hours ago, so opens can come in, and at most 30 days ago. A resend must also start sending
  within those 30 days. These are checked when the resend is made and again when it starts sending.
- At most 2 resends per campaign, and a resend can't itself be resent. The limit holds even when
  two people click at once.
- Deleting a draft resend that never sent frees its slot. A resend that started sending keeps its
  slot for good and can't be deleted (archive it instead); its sends stay in the combined totals.
- Opens are undercounted: some email apps (Apple Mail, for one) hide or fake opens, so some people
  who read the original will get it again. DumbMail doesn't record which opens came from privacy
  proxies, so it can't leave those people out; every recorded open counts.

`GET /api/campaigns/{id}/resends` (MCP `get_campaign_resends`): for an original, its resends with
their stats, combined totals across the original and its resends (people reached, opened and
clicked, each person counted once, all among the people reached) and whether it can be resent now (`can_resend`, with
`earliest_at` and `latest_at`). For a resend, which campaign it resends.

A resend's audience is `{"include": [{"type": "resend", "id": <the resend's own id>}]}`. Only that
resend can use it.

## Tags, archive and search

Tags are free text shared by the account's campaigns, flows and templates: at most 20 on one item,
200 in an account, names up to 40 characters (matched without case).

- `GET /api/tags`: the tags and how many campaigns, flows and templates use each (MCP `list_tags`).
- `POST /api/campaigns/tags`, `/api/flows/tags`, `/api/templates/tags` with
  `{"ids": [...], "add": ["Promo"], "remove": ["Old"]}`: up to 100 items at once; new names become
  tags (MCP `tag_campaigns`, `tag_flows`, `tag_templates`).
- `PATCH /api/tags/{id}` `{"name"}` renames; `DELETE /api/tags/{id}` removes the tag everywhere
  (MCP `rename_tag`, `delete_tag` with `confirm=true`).
- `POST /api/campaigns/archive` (and `/api/flows/archive`, `/api/templates/archive`) with
  `{"ids": [...], "archived": true | false}`. Archived items are hidden from lists unless you ask for
  them, stay in reports, and can't be sent or scheduled (campaigns) or turned on (flows). A
  scheduled, sending or A/B-testing campaign, or a live flow, isn't archived: unschedule, cancel or
  turn it off first. The answer lists what changed and what was skipped, and why. An archived
  campaign can't send tests either. Archived templates still work for transactional sends that name
  them.

The lists take optional filters, all query parameters:

- `GET /api/campaigns`: `tag` (a tag id), `status` (one, or several separated by commas),
  `channel`, `q` (text in the name or subject), `from` and `to` (send date, or creation date for
  drafts without one, YYYY-MM-DD in the account's time zone), `archived` (`false` by default,
  `true`, or `all`), `limit` (up to 500, the default) and `offset`. `total` is the number matching.
  Each campaign also has `tags`, `archived_at` and `resend_of`. MCP: `search_campaigns`.
- `GET /api/flows` and `GET /api/templates`: `tag`, `q` (name), `archived`, `limit`, `offset`;
  flows also `status`, templates also `channel`. A filter a list doesn't have is refused (400).
  MCP `list_templates` leaves archived templates out unless `archived` is `true` or `all`.
- `GET /api/campaigns/counts`: campaigns per status (not archived) and how many are archived.

## Calendar

`GET /api/campaigns/calendar?from=2026-11-01&to=2026-11-30` (at most 62 days; add
`include_archived=true` for archived ones; MCP `campaign_calendar`): campaigns on those days in the
account's time zone. Sent, sending, stopped and failed campaigns are placed by when they started;
scheduled ones and drafts with a planned date by their send time. Each item has its channel,
status, local `date` and `time`, `timing` (`all_at_once`, `local_time`, `best_time`, `gradual`), a
`note` for local-time, best-time and gradual sends (shown on the day they start), and `can_move`.

`POST /api/campaigns/{id}/reschedule` `{"date": "2026-11-20"}` (MCP `reschedule_campaign`, with
`confirm=true`) moves a scheduled campaign to another day and keeps its time of day; a local-time
send moves the day it goes out by the same number of days. It's one write that only succeeds while
the campaign is still scheduled and unchanged since it was read, with both its current and its new
send time more than 15 minutes away; it never sends anything right away. A local-time or best-time
send whose start wouldn't fall on the chosen day in your time zone (each person's local time can
cross a day around a clock change) is refused with a note to pick another day.

## Permissions

Reading any of this needs messaging read (the account defaults: account read). Tags, archive and
moving a campaign need campaign management (flows: flow editing; templates: content writing).
Making a resend draft needs campaign drafting; sending it needs campaign sending, as for any
campaign. A campaign's link tags need campaign drafting, a flow's flow editing, and the account's
defaults settings write. Archiving, unarchiving, moving a campaign, making a resend, deleting a tag
and changing the account's link tag defaults are recorded in the audit log.
