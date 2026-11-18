# Reviews

Ask customers for a review after they buy or stay, collect it on a hosted page, moderate it, show it
on your website and in emails, and use it in segments and flows. It works for shops (the thing
reviewed is a catalog product) and for rental and service businesses (the thing reviewed is a
listing or a service).

## How it works

1. **You ask.** A review request is a signed link for one person and one thing they bought or
   booked. Links come from the `review_request` flow step, the "Review request" email block or the
   `{% review_link %}` tag in any email, or the API (`create_review_request`).
2. **They write.** The link opens a page on your link domain: stars (required), a title, their
   review, up to 5 photos, the name shown with it, whether they'd recommend it, and up to 3 questions
   you set. One review per link and item. They can change it for 24 hours with the same link, and
   delete it at any time.
3. **You moderate.** Verified reviews at or above your star threshold (4 by default) with nothing
   flagged go live at once. Everything else waits in the queue on the Reviews page.
4. **It shows.** On your site through the DumbMail script you already have, in emails through the
   "Reviews" block, and to Google through structured data.

## Asking for reviews

### The starter flows

Reviews → Settings → **After an order** or **After a stay** (or `create_review_flow` with `kind`,
`POST /api/reviews/flows` with `{"kind": "order" | "stay"}`) creates a draft flow:

- **order** (default): `Placed Order`, then wait 14 days. If your account records `Fulfilled Order`,
  `Delivered` or `Delivered Order`, the flow starts from that instead and waits 5 days.
- **stay**: the day after `check_out` at 11am, from the Guesty recipe's `Reservation Confirmed`
  events (Integrations). A booking that's cancelled (`Reservation Cancelled` with the same
  `reservation_id`) leaves the flow. The listing (`listing_id`) is what's reviewed; name it with
  `save_review_product` so the email and widgets show its name.
- Both ask for a review of each item (`review_request` step), wait 7 days, then remind once about
  the items they haven't reviewed (`review_request` with `reminder: true`). Then they stop.

Check the emails and turn it on in Flows. A booking system other than Guesty works the same way
through the API or an integration: the step reads `listing_id`, `listing_name` and
`reservation_id` as well as `items`.

### The `review_request` flow step

```json
{"id": "v1", "type": "review_request", "channel": "email", "reminder": false, "next": null,
 "content": {"subject": "How did we do?", "blocks": [
   {"type": "heading", "text": "How was your order?"},
   {"type": "review_request", "button_text": "Write a review"}]}}
```

- Items come from the event that started the flow: `event.properties.items` in the catalog's item
  shape (`product_id`, `name`, `image_url`, `url`), or the event's own `product_id` / `listing_id`.
  At most 6 items, each with its own link.
- `channel: "sms"` sends a text instead; put `{% review_link %}` in the body (the link is added at
  the end if you leave it out).
- `reminder: true` re-sends only this run's earlier links that are still unused, once.
- Nobody is asked twice about the same order line, nor about something they reviewed in the last 12
  months. An open link for the same item is reused.
- A review request is marketing: it goes only to people **subscribed** on that channel, through the
  normal send path (billing, sending limits, quiet hours, smart sending and the last opt-out check).
  If the message isn't sent, its new links are dropped so the order can be asked about later.
  People held back from marketing (`marketing_hold`, such as guests an integration added) are never
  subscribed, so they're never asked, even in a flow marked as service messages.

### Email blocks and the tag

- **Review request** block (`{"type": "review_request"}`): each item's image and name, five stars
  (each a link that opens the form with that rating chosen; opening it saves nothing) and a button.
  Options: `product_id` (ask about one product instead of the event's items), `heading`,
  `button_text`, `show_stars` (default true).
- `{% review_link %}` is the link for the first item of the triggering event;
  `{% review_link 'PRODUCT-ID' %}` is the link for one product.
- In campaigns, links are made per recipient when the email is sent. People who aren't subscribed
  get the email without the ask (the block is left out and the tag is empty). Previews and test
  sends show a sample link and make nothing.
- **Reviews** block (`{"type": "reviews"}`): a product's star summary and its top published reviews.
  Options: `product_id` (default: the event's first item), `count` (1 to 5), `show` (`both`,
  `summary` or `reviews`). With no published reviews the block is left out. Emails without these
  blocks render exactly as before.

### Verified reviews

A review is **verified** ("Verified buyer") only when its link is tied to a purchase or stay that a
trusted source recorded for that person and that item:

- the flow's (or email's) triggering event was recorded by your server, the API or an integration
  (never the browser), and is a purchase or stay metric: `Placed Order`, `Fulfilled Order`,
  `Delivered`, a metric an integration recipe declares as a purchase (Guesty's `Reservation
  Confirmed`, Stripe's `Placed Order`), or one you list in `purchase_metrics`. Other server-only
  metrics (an inquiry, a subscription) prove nothing; or
- `create_review_request` was given your `order_id` (and not `verified: false`). Without one the
  request is unverified.

Links made from browser events, from other metrics, or from a campaign naming a product still prove
who wrote the review, but not the purchase, so they aren't verified. When a verified request comes
for an order line or item that has an unverified one, the verified one takes its place and the old
link stops working. Imported reviews never get the badge (see Import).

### Where product details come from

Each of a product's name, image and page address is tracked separately: it's trusted only when the
catalog, the API, an integration or a server-recorded event supplied that field. Images and
addresses are loaded or linked (in emails, the review page, the widgets, structured data and the
dashboard) only when trusted. A name a browser sent is shown as text, and only to the person whose
own event carried it. A trusted source vouches only for the fields it sends.

### Links

A link lasts 60 days (`request_days`, 7 to 365) and writes one review per item, ever: if the review
is deleted later (by you or its author), the link can't write another. It's a bearer link: the page
shows only the item's name and (trusted) image, nothing about the person (no name filled in, nothing
about their subscriptions), never an address, order total or account details. During the 24-hour
edit window, the review's own words show only in the browser that wrote it (a cookie set when it
was submitted); anyone else with the link sees that it was reviewed and can delete it. Every answer
carries `Cache-Control: no-store` and `Referrer-Policy: no-referrer`.

### One ask, one message

Each ask, reminder or API "send now" is recorded before anything is sent, and its id is the
message's id. Each request is in at most one ask and one reminder, however many sends race for it
(a unique key decides), and an API "send now" queues the job that sends it in the same step that
records it, so a call that stops right after still sends, once. A step that runs again (a retry, a worker that stopped) finds the message it already
handed over and doesn't send another; a worker that stopped before the hand-off is replaced after
10 minutes, and the ask goes once. A request counts as sent only once its message was handed over.
If the message is skipped or fails, its new requests are dropped, so the order line can be asked
about later. Right before an ask goes, DumbMail checks again that the person is subscribed on that
channel, not suppressed and not held back from marketing, with no exception for service-message
flows, and leaves out items reviewed in the meantime; a reminder whose items were all reviewed
isn't sent at all. The sender's own last check, right before the hand-off to the provider, applies
the same strict rules again (email needs subscribed; a hold has no exception), and refuses a
reminder once any of its items has been reviewed. A step resumed after a stop reads its requests
back from its record, so the reminder still follows.

## The review page

- Mobile-first, on your link domain, in your brand colour and logo.
- Text is stored as plain text and escaped wherever it's shown. No HTML, and links in reviews aren't
  linked.
- Photos: up to 5, PNG, JPEG, GIF or WebP, up to 10 MB each. Each is re-encoded as a JPEG of at most
  1600 pixels a side, which drops EXIF data (including location). A photo's declared size is checked
  (at most 40 million pixels) and the five-photo limit applied before anything is decoded, and only
  a few decodes run at once per account and per server. Photos are served at `/i/rv/{id}.jpg` with
  an unguessable id, only while their review is published. Removing a photo or unpublishing the
  review stops it being served; copies already cached by browsers or proxies can still show for up
  to five minutes.
- Left empty, the name shown is never taken from the person's profile: "A verified buyer" for a
  verified purchase, "A guest" for a stay, otherwise "A customer".
- A review never subscribes anyone. If you pick a sign-up form (`subscribe_form_id`), the page offers
  an unticked box; ticking it signs them up through that form, with its list's double opt-in if it
  has one.
- Rate limits: 30 writes an hour per link and 120 an hour per visitor address.
- If you set a thank-you coupon, the page says that people who get your emails receive a code for
  any review, whatever the rating.

### Public reviews (no link)

Off by default (`public_reviews`). When on, the product widget shows "Write a review". These
reviews are **unverified**, count only after the writer confirms their email address by tapping a
button in a confirmation email, and are always held for moderation. Limits: 5 an hour per visitor,
30 an hour per product, one review waiting for confirmation per address per account (addresses are
compared without case, dots in Gmail addresses or plus tags), and 3 confirmation emails per address
a day across all accounts. The answer is the same whatever the address (a customer or not,
suppressed or not, your sending paused or not), the email goes from a background job, and no profile
is created or changed until the link is used. Unconfirmed reviews are deleted after 7 days. They
can't carry photos.

## Moderation

Statuses: `pending`, `published`, `rejected`, `removed` (deleted by its author).

- Auto-publish (`auto_publish`, `auto_publish_min_rating`): verified reviews with at least that many
  stars and nothing flagged. Everything else is held.
- Flags: profanity, email addresses, phone numbers and links in the title, text, name and answers, plus your own
  `flag_words`. A flagged review is held and shows why.
- The queue filters by status, rating, product, verified, photos, source and date (days in your
  account's time zone). Bulk publish and reject, feature (pin), reply, remove single photos.
- Rejecting needs a reason: `spam`, `offensive`, `personal_info`, `not_about_this`, `fake` or
  `other`. It never deletes. Deleting is separate, can't be undone and needs confirming.
- Every moderation action is in the audit log, written in the same transaction as the change, with the ids it changed.

### Honesty rules (enforced)

- Publishing can't depend on the rating beyond the hold rule above. There's no setting that hides
  low ratings, and the queue reminds you that suppressing negative reviews is unlawful in the US (FTC
  rule on consumer reviews and testimonials) and the EU.
- A thank-you coupon (`thank_you_coupon`, a coupon's name) is emailed for any review, whatever the
  rating, to reviewers subscribed to email. The form says so before they write, and those reviews
  are labelled "Received a discount for this review" wherever they're shown.
- The business can't edit a review, only reply to it (shown under it as "Reply from the store").
- Authors can delete their own review from their link. `features::reviews::store::delete_for_profile`
  deletes everything a person wrote (reviews and photos), and deleting a profile does the same.

## On your website

The DumbMail script you already installed loads the widgets on pages that have one. No second script
tag. Put a `<div>` where each widget goes:

```html
<!-- Product reviews: summary, histogram, sort and filter, photos, pages, replies -->
<div data-dumbmail-reviews="PRODUCT_ID" style="min-height:360px"></div>

<!-- Star rating badge for product cards and lists -->
<span data-dumbmail-stars="PRODUCT_ID"></span>

<!-- Carousel of featured reviews (the newest when none are featured) -->
<div data-dumbmail-reviews-carousel data-limit="10"></div>

<!-- Every product's reviews, for a "Reviews" page -->
<div data-dumbmail-reviews-all></div>
```

`PRODUCT_ID` is the same id your catalog or order events use (on Shopify, the product id; for a
rental, the listing id). Each widget draws in its own shadow root, in your brand colours (change them
under Reviews → Settings), works with the keyboard and screen readers, keeps its space while loading
(set a `min-height` like above so nothing moves before the script runs) and loads when it's near the
screen. Widget traffic isn't billed.

- **Shopify:** add a Custom Liquid section or block to the product template with
  `<div data-dumbmail-reviews="{{ product.id }}"></div>`, and
  `<span data-dumbmail-stars="{{ product.id }}"></span>` in the product card snippet. A theme app
  embed block isn't available yet.
- **WooCommerce / WordPress:** a Custom HTML block on the product template, with the product's id.
- **Any other site:** paste the `<div>` in the page's HTML.

### Structured data

On a product page, the product widget adds Google's review snippet markup (`Product` with
`AggregateRating` and up to 10 `Review`s), built from the same answer as the reviews it shows (the
first page, default order, no filters): the marked-up reviews are the ones shown there, and the
`AggregateRating` is exactly the rating the widget shows. Imported reviews show in the widget but are
never marked up: Google doesn't allow marking up reviews gathered on other sites. So a product with
imported reviews gets no `AggregateRating` at all (the widget's rating counts them); its reviews
collected here are still marked up. The JSON-LD endpoint answers a matching `If-None-Match` from
the product's summary version, before reading any review. It's added only
when the product's name came from you (catalog, API or a server event) and it is a `product`. Listings and services get none:
Google doesn't show stars for a business's reviews of itself. Turn it off with
`data-structured-data="false"`, or fetch it for your own page template from
`GET /client/reviews/jsonld?k=SITE_KEY&product_id=ID`.

### Public endpoints (by site key)

| Method | Path | Returns |
| --- | --- | --- |
| GET | `/client/reviews?k&product_id&sort&rating&photos&after` | `{product, summary: {average, count, histogram}, reviews, next, per_page, widget, can_write, questions, structured_data}`. `sort`: `recent`, `highest`, `lowest` (default: featured first, then newest). Pass `next` as `after` for the next page. |
| GET | `/client/reviews/summary?k&ids=A,B,C` | `{data: {id: {average, count}}}` for up to 50 products. |
| GET | `/client/reviews/all?k&after&featured=true&limit` | Every product's published reviews (or the featured ones), with each product's trusted name. |
| GET | `/client/reviews/jsonld?k&product_id` | The structured data (404 when there's none). |
| POST | `/client/reviews/write?k` | A public review `{product_id, email, rating, title, body, name}` (when turned on). |

Only published reviews leave these endpoints, with the display name, date, rating, text, answers,
photos and reply; never an email address, a profile id or anything unpublished. Imported reviews
never carry the verified badge here. Answers carry an ETag and `Cache-Control: public, max-age=60`;
a matching `If-None-Match` is answered from the product summary's version before any list is read.
Lists are keyset pages and never count rows (counts come from the stored summary), and reads are
limited to 300 a minute per visitor address. A product's name, picture and page address appear only
when they came from a trusted source.

## Segments and flows

Segment condition `review`:

```json
{"type": "review", "action": "reviewed", "op": "has", "product_id": "SKU-1", "min_rating": 4, "max_rating": 5, "count": 2, "within_days": 90}
{"type": "review", "action": "pending_request", "op": "has_not"}
```

`action` is `reviewed` (default) or `pending_request` (has an unused, unexpired link); `op` is `has`
or `has_not`; the rest are optional. Reviews waiting for their email confirmation don't count.

Events (only DumbMail records these; a browser or the API can't send them):

- `Ready To Review` when someone is asked: `product_id`, `product_name`, `request_id`, `verified`, `source`.
- `Submitted Review` when a review is written (or a public one confirmed): `rating`, `product_id`,
  `product_name`, `has_photos`, `verified`, `review_id`, `source`.

## Reports

`GET /api/reviews/report?days=90` (`get_review_report`): requests sent, how many were answered and
the response rate, reviews written and their average rating, a daily series (account time zone),
counts by status, and each product's reviews and average (all written reviews, and published only).
Imported reviews are left out of the written counts. Reviews' share of attributed revenue isn't
reported.

## Import and export

- **Import** (`import_reviews_csv`, `POST /api/reviews/import` with `{csv, mapping, publish, confirm}`):
  a CSV export from another reviews app. Columns are matched by common names (product id or handle,
  rating, title, body, name, email, date, verified, photo URLs, reply) and you can map them yourself.
  Without `confirm` it's a dry run: the mapping, a sample and the rows it would skip. It then runs in
  the background (`get_review_import`). Up to 20,000 rows and 10 MB. Imported reviews keep the date
  and name the file gives, are published (or held, with `publish: false`) and are marked "Imported"
  in the dashboard. They never get the public "Verified buyer" badge: what the file said is kept as
  "marked verified by the source", shown in the dashboard and the export only. Photo addresses are
  fetched through the checked path (public addresses only, timeouts, size caps) and re-encoded like
  uploads; each row is saved together with its photos, and the next batch is queued with the
  progress, so an import that stops picks up where it left off without skipping a photo or a row. A
  reviewer's email links the review to an existing profile; it never creates one.
- **Export** (`export_reviews_csv`, `GET /api/reviews/export` with the list filters): CSV of up to
  50,000 reviews. `with_people=true` adds each reviewer's email; that needs an Admin or Owner
  (`exports.manage`) and profiles read as well as `export`. Every cell a person typed (names, text,
  replies, emails) is guarded so a spreadsheet won't read it as a formula.

## API and MCP

| Method | Path | MCP tool | Permission |
| --- | --- | --- | --- |
| GET | `/api/reviews` | `list_reviews` | content.read (+ profiles.read for who wrote them) |
| GET | `/api/reviews/{id}` | `get_review` | content.read |
| POST | `/api/reviews/moderate` `{ids, action, reason, confirm}` | `moderate_reviews` | reviews.manage |
| POST | `/api/reviews/{id}/reply` `{text}` | `reply_to_review` | reviews.manage |
| DELETE | `/api/reviews/{id}/photos/{photo_id}` | `remove_review_photo` | reviews.manage |
| DELETE | `/api/reviews/{id}?confirm=true` | `delete_review` | catalog.delete |
| GET / PATCH | `/api/reviews/settings` | `get_review_settings`, `update_review_settings` | content.read / reviews.manage |
| GET / POST | `/api/reviews/products` | `list_review_products`, `save_review_product` | content.read / reviews.manage |
| POST | `/api/reviews/products/rebuild` | `rebuild_review_summaries` | reviews.manage |
| POST | `/api/reviews/requests` | `create_review_request` | reviews.manage and profiles.read (+ campaigns.send to send now) |
| GET | `/api/reviews/report` | `get_review_report` | analytics.read |
| POST | `/api/reviews/import`, GET `/api/reviews/imports/{id}` | `import_reviews_csv`, `get_review_import` | catalog.delete |
| GET | `/api/reviews/export` | `export_reviews_csv` | export (+ exports.manage and profiles.read with `with_people`) |
| POST | `/api/reviews/flows` | `create_review_flow` | flows.edit |

`reviews.manage` belongs to Managers, Admins and Owners. `moderate_reviews` needs `confirm: true` to
publish or reject more than one review; `delete_review` and `import_reviews_csv` always do;
`create_review_request` does when it sends.

Star summaries (count, average, histogram) are kept per product and changed in the same transaction
as every publish and unpublish. `rebuild_review_summaries` recounts them from the reviews in the
background.
