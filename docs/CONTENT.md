# Email content

More email blocks, display rules, saved blocks, starter templates and checks before sending. The core blocks (heading, text, image, button, divider, spacer, columns, product, products, html) are in `dumbmail://docs/email`.

Text in any block can use personalization tags like `{{ first_name|default('there') }}`. Links and image addresses in these blocks can be tags too, like `{{ event.properties.url }}`: they're filled in for each recipient and then checked. A link must come out as `https://`, `http://`, `mailto:` or `tel:`, and an image as `https://` or `http://`; anything else (javascript:, data:, an address that's empty for this person) shows the block without that link or image.

Email that uses none of this page's features (saved blocks, display rules, the blocks below) is rendered exactly as before they existed.

## Blocks

### Social links

```json
{"type": "social", "links": [{"network": "instagram", "url": "https://instagram.com/acme"}, {"network": "email", "url": "hello@acme.com"}],
 "style": "color", "size": 32, "align": "center"}
```

- `network`: `instagram`, `facebook`, `tiktok`, `x`, `youtube`, `pinterest`, `linkedin`, `threads`, `website` or `email`.
- Leave `links` out to use the account's social links (Settings → Brand).
- `style`: `color` (each network's color), `dark`, `light`, or `white` (for dark backgrounds). `size`: 24, 32 or 40.
- Icons are PNGs hosted by DumbMail, not loaded from the networks.

### Video

```json
{"type": "video", "url": "https://www.youtube.com/watch?v=...", "thumbnail": "https://.../i/...", "alt": "Watch the launch", "caption": "1 minute"}
```

Email apps can't play video, so this is a picture with a play button that opens the video. Make the picture with `POST /api/content/video-thumbnail` (MCP `make_video_thumbnail`): give `video_url` for a YouTube or Vimeo video, or `image_url` for any image (for other video sites). It draws the play button on the picture and saves it with your images. Without a thumbnail the block shows a dark panel with a play link.

### Countdown timer

```json
{"type": "countdown", "ends_at": "2026-12-01T08:00:00-08:00", "background": "#111827", "color": "#ffffff", "label_color": "#9ca3af",
 "ended_text": "This sale has ended"}
```

An animated GIF drawn each time the email is opened: days, hours, minutes and seconds left, counting down for one minute and then holding. After `ends_at` it shows `ended_text` (letters, numbers and basic punctuation, up to 40 characters). Some email apps show only the first frame, which still shows the time left when the email was opened.

### Table

```json
{"type": "table",
 "columns": [{"label": "Item"}, {"label": "Qty", "align": "center"}, {"label": "Price", "align": "right"}],
 "rows": [["Gift wrap", "1", "$0.00"]],
 "loop": {"source": "event.properties.items", "cells": ["{{ item.name }}", "{{ item.quantity }}", "{{ item.price }}"]},
 "footer": [["Total", "", "{{ event.value }}"]]}
```

Up to 6 columns and 50 rows. `loop` adds one row per entry in a list from the triggering event (up to 50); each entry is `item`. Use it for order summaries in flows. If the event has no such list, no rows are added.

### Quote

```json
{"type": "quote", "text": "Best purchase all year.", "author": "Jordan P.", "role": "Customer", "image": "https://...", "style": "card"}
```

`style`: `plain` (a colored bar on the left, `color` sets it; defaults to the brand color) or `card` (a light box, centered).

### Header, menu bar and footer

```json
{"type": "header", "logo": "https://.../logo.png", "logo_width": 140, "logo_href": "https://acme.com", "links": [{"text": "Shop", "href": "https://acme.com/shop"}]}
{"type": "nav", "links": [{"text": "New", "href": "..."}, {"text": "Sale", "href": "..."}], "background": "#1f6f5c", "color": "#ffffff", "separator": true}
{"type": "footer", "text": "Questions? Reply to this email.", "links": [{"text": "FAQ", "href": "..."}], "social": [{"network": "instagram", "url": "..."}], "show_social": true, "style": "light"}
```

The account's logo already shows above every email; give a header `logo` only for a different picture. Up to 8 links each. The footer shows social icons (the account's, unless `social` lists them) above its text and links. The required footer with the mailing address and unsubscribe link is still added below everything.

### Image beside text

```json
{"type": "image_text", "src": "https://...", "alt": "Our tote", "href": "https://...", "heading": "Built to last", "html": "<p>Two sentences.</p>",
 "button_text": "Shop", "button_href": "https://...", "image_position": "left", "image_width": 40, "radius": 6}
```

Side by side on wide screens; on phones the image goes on top. `image_width`: 33, 40 or 50 (percent).

### Images

Image blocks also take `radius` (0 to 40 px, default 6) for rounded corners. Give every image `alt` text: it's read aloud by screen readers and shown when images are blocked.

## Display rules

Any block can carry `show_if`, so one email varies by person:

```json
{"type": "text", "html": "<p>Your VIP code: VIP20</p>", "show_if": {"field": "person.properties.tier", "op": "eq", "value": "gold"}}
```

- `field`: a dotted path in the personalization data: `first_name`, `email`, `person.properties.<key>`, `event.value`, `event.properties.<key>`.
- `op`: `set`, `not_set`, `eq`, `neq`, `contains` (text contains, or a list includes), `gt`, `lt` (numbers; text that isn't a number never matches). Text comparisons ignore case.
- Several rules that must all be true: a list, up to 5: `"show_if": [{"field": ...}, {"expr": ...}]`.
- Advanced: `{"expr": "event.value > 100 and person.properties.vip"}`, a template expression that compares and combines values: `and`, `or`, `not`, `==`, `<`, `in`, `is defined`, and the filters `int`, `float`, `lower`, `upper`, `length`, `count`, `default`, `trim`, `first`, `last`, `string`, `bool`, `abs`, `round`, `title`, `capitalize`. No `{`, `}`, `%`, `#`, `*`, `~` or function calls. Each rule runs with a fixed work budget and sees only that recipient's data.

Rules are checked for each recipient when the email is sent, and for the chosen person in previews ("Preview as"; the preview with no person picked shows every block when the sample person would see none). On a saved block reference, the rule applies to the whole saved block; detaching it puts the rule on each block it held.

A rule never fails a send:

- A rule that can't be worked out for someone (an advanced rule comparing text with a number, say) hides its block for that person only.
- When rules hide every block for someone, that person is skipped with the reason "nothing to show for this person" (shown in the campaign report's delivery card and in `get_send_progress`); nobody gets an empty email. A test send to such a person says so.
- The check (below) says how many people in the audience every block would be hidden for, and how many a rule can't be worked out for.

## Saved blocks

A saved block is a named group of blocks (a header, a footer, a sign-off) reused across emails.

- `GET /api/content/saved-blocks`: list, with `uses` (places in unsent emails that use each).
- `POST /api/content/saved-blocks` `{"name", "blocks": [...]}`: up to 50 blocks; names are unique per account; up to 200 saved blocks.
- `GET|PATCH|DELETE /api/content/saved-blocks/{id}`, `GET /api/content/saved-blocks/{id}/usage`.

Put one in an email:

```json
{"type": "saved", "saved_block_id": "0193...", "name": "Footer", "blocks": [ ...a copy... ]}
```

The email shows the saved block's current content. Editing a saved block changes every template, draft or scheduled campaign and flow email that uses it.

- An edit is refused (409, naming them) when it would stop an email that's set to go out without anyone looking at it again from sending: a scheduled or starting campaign (or one of its A/B versions) or a live flow. For example, a change that makes the saved blocks in one email add more than 300 blocks, or text that doesn't render.
- When a campaign's send starts (in the same step that fixes who gets it), the saved blocks in it and every A/B version are copied in from one read (`"frozen": true`) and checked as copied. Everyone gets that copy, also people whose email waits for their local or best time. A copy that can't be sent fails the campaign before anyone gets it.
- Duplicating a campaign gives a new, unsent email: its saved blocks show their current content again.
- To edit one email's copy only, detach it: replace the `saved` block with what it shows now (`GET /api/content/saved-blocks/{id}?include_deleted=true` gives the content even after the saved block was deleted).

Deleting a saved block removes it from the list; emails that still use it keep its last content. A saved block can't contain another saved block.

MCP: `list_saved_blocks`, `save_block`, `update_saved_block`, `delete_saved_block` (needs `confirm: true`).

## Starter templates

`GET /api/content/starters` (`?preview=true` adds each one's rendered HTML) lists 14 designs: welcome, newsletter, product launch, sale, abandoned cart, order confirmation, shipping update, review request, win-back, birthday, back in stock, personal note, event or booking confirmation, and pre-arrival info. They use the account's brand color, name, website and social links. Flow starters read the triggering event (`event.properties.items`, `order_id`, `tracking_url`, ...) and are previewed with a sample event. Gray sample images mark where your own pictures go.

- `GET /api/content/starters/{key}`: one starter's `content` (use it in a campaign or flow email).
- `POST /api/content/starters/{key}/template` `{"name"}`: save it as a template.

Links that fall back to your website read it when the email renders (`organization.website`), so the address isn't copied into the template.

The pre-arrival starter is for stays. Start its flow from a booking event your booking system sends through the API or an integration, never from an event your website sends from the browser (anyone can send those). Have the event carry `paid_in_full`: the intro, the arrival table (check-in, check-out, address, parking) and the directions button show only when `event.properties.paid_in_full` is true; otherwise the email says the details come once the booking is paid in full. It has no door, lockbox or access codes.

MCP: `list_starter_templates` (with `key` for one's content), `create_template_from_starter`.

## Checks before sending

`POST /api/content/check` (MCP `check_email`) with one of:

- `{"content": {...}}`: any email;
- `{"campaign_id": "..."}`: a campaign (optionally with an unsaved `content` or `audience`);
- `{"flow_id": "...", "node_id": "..."}`: a flow's email step.

It returns `findings`, each with a `severity` (`error`, `warning`, `info`), a `title`, a `detail` that says what to fix, and `blocks_sending` (true only for what the product already refuses to send: a missing subject, sender or mailing address, an unverified sending domain where one is required, an empty audience, content that doesn't render). It checks:

- links and images: broken or unreachable (requested from DumbMail's servers: public addresses only, redirects not followed and so reported as not checked, up to 40 addresses, results kept for 10 minutes; `skip_links: true` skips this), image addresses that answer with a web page instead of an image, images over 1 MB, missing alt text, sample images left in. DumbMail's own addresses (tracking links and open pixels copied from a sent email, unsubscribe links, hosted images) on any DumbMail host or any account's branded link domain are never requested, since that would count as an open or click: they're listed as "DumbMail link, not checked", and a tracked link is checked as the address it leads to;
- subject (missing, over 60 characters, all capitals, `!!`) and preview text (missing, too long, same as the subject);
- Gmail clipping: the email as sent (with tracked links) over 102 KB;
- personalization without a fallback, with how many recipients it would be empty for (in a campaign's audience, or everyone who can get email); event data in a campaign, where there is no event;
- who it goes to: nobody reachable, and how many never subscribed;
- display rules: how many people every block would be hidden for (they'd be skipped), and how many a rule can't be worked out for (tried on up to 5,000 people in the audience; for flows, only rules about the person);
- spam signals: image-only emails, link shorteners, link text that shows a different address than the link opens;
- whether the sending domain is authenticated.

`recipients` is the reachable count, `size_bytes` the email's size, and `links` each address checked with its status.

One check runs at a time per account; counting the audience and trying rules on it run behind that, a few at a time across the server, within 15 seconds.

## Hosted images

Social icons (`/e/icons/...`), starter sample images (`/e/assets/...`) and countdown images (`/e/countdown/...`, signed) load on every DumbMail host: the app, the marketing site, retired hosts, the shared link host and branded link domains. Countdown images are drawn once per timer per second and shared by every open in that second, at most 4 at a time and 16 waiting across the server, and 240 new drawings a minute per account. Video thumbnails (`make_video_thumbnail`) take pictures up to 24 megapixels, come out at most 1200 px on either side, and are made one at a time per account (two at a time across the server).

## Transactional email

Transactional email (`dumbmail://docs/transactional`) renders on its own path, so it takes the core blocks and the `table` block only. Saved blocks, display rules and the other blocks on this page are refused when a transactional template is saved and when one is sent.

## Who may use what

Each route and tool needs a permission (see `dumbmail://docs/team`; API keys need a scope that covers it):

| Route or tool | Needs |
| --- | --- |
| `GET /api/content/saved-blocks`, `GET /api/content/saved-blocks/{id}`, `GET .../usage`, `list_saved_blocks` | `content.read` |
| `GET /api/content/starters`, `GET /api/content/starters/{key}`, `list_starter_templates` | `content.read` |
| `POST /api/content/saved-blocks`, `PATCH /api/content/saved-blocks/{id}`, `save_block`, `update_saved_block` | `content.write` |
| `POST /api/content/starters/{key}/template`, `create_template_from_starter` | `content.write` |
| `POST /api/content/video-thumbnail`, `make_video_thumbnail` (stores an image; requests the address) | `content.write` |
| `DELETE /api/content/saved-blocks/{id}?confirm=true`, `delete_saved_block` with `confirm: true` (without it, a dry run that says where it's used) | `content.write`, like deleting a template |
| `POST /api/content/check`, `check_email` | `content.write`; counting recipients and trying personalization and display rules on them also needs `profiles.read` (without it those findings are left out and `people_not_checked` says so) |
| `/e/icons/{file}`, `/e/assets/{file}`, `/e/countdown/{file}` | none (images in sent email; countdown addresses are signed) |

Previewing as a real person (`POST /api/render` with `profile_id`) needs `profiles.read`; without it the preview uses the sample person. Saved-block changes and templates made from starters are recorded in the audit log.
