# Email content

More email blocks, display rules, saved blocks, starter templates and checks before sending. The core blocks (heading, text, image, button, divider, spacer, columns, product, products, html) are in `dumbmail://docs/email`.

Text in any block can use personalization tags like `{{ first_name|default('there') }}`. Links may start with `https://`, `http://`, `mailto:` or `tel:`, or be a tag like `{{ event.properties.url }}`; anything else is dropped.

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
- Advanced: `{"expr": "event.value > 100 and person.properties.vip"}`, any template expression (no `{`, `}`, `%` or `#`). An expression that refers to something missing can fail to render, so guard it with `is defined`.

Rules are checked for each recipient when the email is sent, and for the chosen person in previews. On a saved block reference, the rule applies to the whole saved block.

## Saved blocks

A saved block is a named group of blocks (a header, a footer, a sign-off) reused across emails.

- `GET /api/content/saved-blocks`: list, with `uses` (places in unsent emails that use each).
- `POST /api/content/saved-blocks` `{"name", "blocks": [...]}`: up to 50 blocks; names are unique per account; up to 200 saved blocks.
- `GET|PATCH|DELETE /api/content/saved-blocks/{id}`, `GET /api/content/saved-blocks/{id}/usage`.

Put one in an email:

```json
{"type": "saved", "saved_block_id": "0193...", "name": "Footer", "blocks": [ ...a copy... ]}
```

The email shows the saved block's current content. Editing a saved block changes every template, draft or scheduled campaign and flow email that uses it. When a campaign starts sending, the saved block's content is copied into it (`"frozen": true`), so later edits don't change what it sent. To edit one email's copy only, detach it: replace the `saved` block with its `blocks`. Deleting a saved block removes it from the list; emails that still use it keep its last content. A saved block can't contain another saved block.

MCP: `list_saved_blocks`, `save_block`, `update_saved_block`, `delete_saved_block` (needs `confirm: true`).

## Starter templates

`GET /api/content/starters` (`?preview=true` adds each one's rendered HTML) lists 14 designs: welcome, newsletter, product launch, sale, abandoned cart, order confirmation, shipping update, review request, win-back, birthday, back in stock, personal note, event or booking confirmation, and pre-arrival info. They use the account's brand color, name, website and social links. Flow starters read the triggering event (`event.properties.items`, `order_id`, `tracking_url`, ...) and are previewed with a sample event. Gray sample images mark where your own pictures go.

- `GET /api/content/starters/{key}`: one starter's `content` (use it in a campaign or flow email).
- `POST /api/content/starters/{key}/template` `{"name"}`: save it as a template.

MCP: `list_starter_templates` (with `key` for one's content), `create_template_from_starter`.

## Checks before sending

`POST /api/content/check` (MCP `check_email`) with one of:

- `{"content": {...}}`: any email;
- `{"campaign_id": "..."}`: a campaign (optionally with an unsaved `content` or `audience`);
- `{"flow_id": "...", "node_id": "..."}`: a flow's email step.

It returns `findings`, each with a `severity` (`error`, `warning`, `info`), a `title`, a `detail` that says what to fix, and `blocks_sending` (true only for what the product already refuses to send: a missing subject, sender or mailing address, an unverified sending domain where one is required, an empty audience, content that doesn't render). It checks:

- links and images: broken or unreachable (requested from DumbMail's servers: public addresses only, redirects reported but not followed, up to 40 addresses, results kept for 10 minutes; `skip_links: true` skips this), images over 1 MB, missing alt text, sample images left in;
- subject (missing, over 60 characters, all capitals, `!!`) and preview text (missing, too long, same as the subject);
- Gmail clipping: the email as sent (with tracked links) over 102 KB;
- personalization without a fallback, with how many recipients it would be empty for (in a campaign's audience, or everyone who can get email); event data in a campaign, where there is no event;
- who it goes to: nobody reachable, and how many never subscribed;
- spam signals: image-only emails, link shorteners, link text that shows a different address than the link opens;
- whether the sending domain is authenticated.

`recipients` is the reachable count, `size_bytes` the email's size, and `links` each address checked with its status.
