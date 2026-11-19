# Customer hub

The customer hub is an account page for your shoppers, on your own site's address. Shoppers sign in with a code sent to their email and see:

- **Orders**: their orders from the last two years, with items, totals, status (placed, paid, shipped, delivered, refunded, cancelled) and parcel tracking.
- **Bookings and subscriptions**: the custom object records you choose (reservations, memberships, subscriptions) that are linked to them, showing only the fields you choose.
- **Picked for you**: product picks for them, and their back-in-stock requests (they can cancel one).
- **Email and texts**: their email and text subscriptions and your preference-page lists.
- **Help**: your questions and answers and a contact link.

Turn it on in **Settings → Customer hub**, or with the `update_hub_settings` MCP tool or `PATCH /api/hub/settings`.

## Where it lives

The hub's address is `https://<your link domain>/hub/<site key>`.

- With a **branded link domain** (Settings → Sending, for example `links.yourbrand.com`), the hub is on that domain. When the link domain is part of your site's domain (your website is `yourbrand.com`), the hub opens in a panel on your pages. Only the sites you approve may show the panel (`panel_origins`; your website's exact address by default). Every other page of the hub refuses to be shown inside another site.
- Without one, the hub is on DumbMail's shared link address and opens **in a new tab**. Browsers block sign-in cookies inside a panel from another site, so a panel can't work there.

## Opening the hub from your site

When a launcher is on, the website snippet loads it (the storefront settings it already fetches say so; with the hub or the launcher off, the snippet loads nothing extra). Choose how shoppers open it:

- **Off**: no launcher. Link to the hub's address yourself.
- **Floating button**: a "My account" button in the corner of every page that has the snippet.
- **Your own link or button**: add `data-dumbmail-hub` to any link or button, or call `DumbMail.openHub()`:

```html
<a href="#" data-dumbmail-hub>My account</a>
<button type="button" onclick="DumbMail.openHub()">My account</button>
```

`dumbmail.openHub()` works too. Both do nothing while the launcher is off.

## Signing in

- **Code by email.** The shopper enters their email; if you have a profile with that address, DumbMail emails a 6-digit code from your sending address. The code works for 10 minutes and allows 5 tries; asking again sends a new code and the old one stops working. The page says the same thing whether or not the address is known. Code requests are limited per address (5 an hour), per visitor (20 an hour) and per account (500 an hour).
- **Sign-in link.** Use `{{ hub_url }}` in an email: the recipient gets a link that signs them in with one button press ("Continue to your account"), so a mail scanner opening the link can't use it. A link works once, for 72 hours, and only until the person's email changes or they sign out everywhere; after that it falls back to the code. Only emails get sign-in links: texts, WhatsApp, push, test sends and previews get the plain address. The link is never sent through click tracking, so it isn't kept in click reports.

The website snippet's visitor tracking never signs anyone in.

Code emails are service messages: they aren't billed, don't count as campaigns and are recorded as transactional. Just before one goes out it's checked again: the hub is still on, the code is still the newest for that address and unexpired, your sending isn't paused, and the address hasn't bounced or reported spam.

A session lasts 30 days. It ends when the shopper signs out. **Sign out on all devices**, a change of the person's email (even one changed back later) and deleting the profile end every session, sign-in link and waiting code for good. A merge must do the same for both people, with `features::hub::end_sessions(db, profile_id)`.

## Whose orders show

Signing in proves the shopper owns the inbox. Orders, bookings, picks and back-in-stock requests show only when the person's email came from your own systems: the API, MCP, the dashboard, an import, a connected integration, Shopify or a Klaviyo import (`profiles.email_source`). An email the website snippet or a form filled in on a profile (one your systems knew only by phone, say) opens only the person's email preferences, with a note. Addresses saved before this rule have no source until your systems send them again.

## What shoppers see

### Orders

Orders come only from order events your systems recorded: Shopify, WooCommerce, BigCommerce, Square, Paddle, AfterShip, and the API or MCP. Events from the website snippet never show, so nobody can add orders to their own page.

An order is a `Placed Order` event with an `order_id`. These events update it when they carry the same `order_id` (or `order_number`):

| Event | Shows as |
|---|---|
| `Placed Order` | Order placed; **Paid** when `financial_status` is `paid` (or `paid: true`), or it came from a store that records orders once paid |
| `Fulfilled Order`, `Fulfilled Partial Order` | Shipped (or partly shipped) |
| `Shipment In Transit`, `Out For Delivery`, `Delivered`, `Shipment Exception` | Parcel status; `Delivered` marks the order delivered |
| `Refunded Order` | Refunded when the refunds reach the total, else the amount refunded |
| `Cancelled Order` | Cancelled |

Properties read from `Placed Order`: `order_id`, `order_name` or `order_number`, `total`, `currency`, `financial_status`, `fulfillment_status`, and `items` (each with `name`, `quantity`, `price`, `image_url`, `url`, `product_id`, `sku`). Items that match your product catalog get its picture and link. Tracking comes from shipment events with `tracking_number`, `tracking_url`, and `carrier` (or `tracking_company`, `courier`). Send these with the API to add tracking for any store.

### Bookings and subscriptions

Choose object types and, for each, the fields to show and what kind it is (booking, subscription or other). Only records linked to the shopper show.

- A field marked **sensitive** never shows, even if you choose it.
- Numbers, dates, amounts, yes/no and fixed choices show when you choose them.
- **Free-text** fields (text, lists, choices without fixed options), the record's title included, show only when you also tick, per field, "never contains an address, door code, wifi password or other access detail" (`cleared`). The tick lapses when the field's definition changes (its label, type or options); tick it again to keep showing it.
- The title shows only when you choose its field, under the same rules.
- For a **booking**, fields that look like access or address details (address, door or gate codes, lock, key, Wi-Fi, password, parking, directions, unit and so on) never show, ticked or not.

### Picked for you

Recommendations for the shopper (bought or viewed together with what they bought or viewed, then popular products), and their back-in-stock requests.

### Email and texts

The same choices as the preference page your emails link to:

- **Unsubscribe from all email**, or choose preference-page lists while subscribed.
- **Get our emails** for someone not subscribed. This is their own opt-in for the address they just proved they own. An address that bounced or was reported can't be changed.
- **Stop texts**. Texts can't be turned on here: the sign-in code proves the email address, not the phone number.

Opt-outs and list choices go through the preference page's own functions and record the same events (`Unsubscribed`, `Unsubscribed from SMS`, `Unsubscribed from List`) with `method: hub`; a re-subscribe records `Subscribed to Email Marketing`. The page says what actually happened ("Nothing was changed" when nothing was).

## Events

| Event | When | Properties |
|---|---|---|
| `Opened Hub` | A signed-in shopper opens the hub (once a day per session) | `embed` |
| `Viewed Order in Hub` | They open an order (once a day per order and session) | `order_id`, `order_number`, `status` |

Only DumbMail records these. The API, MCP and the website snippet can't send them. Use them to start flows or build segments.

## Settings

`GET /api/hub/settings` (account read) and `PATCH /api/hub/settings` (settings write). The MCP tools are `get_hub_settings` and `update_hub_settings`. Send only the fields that change:

| Field | Meaning |
|---|---|
| `enabled` | On or off. |
| `launcher` | `off`, `button` or `link`. |
| `button_label` | The button's text (up to 40 characters). |
| `sections` | `{orders, bookings, recommendations, preferences, help}`, each true or false. |
| `object_types` | Up to 10 of `{type, kind, fields, cleared}`: an object type key, `booking`, `subscription` or `other`, up to 20 field keys, and the free-text fields you confirm never hold access details. |
| `help` | Up to 20 of `{question, answer}`. |
| `contact_url`, `contact_label` | Where "Contact us" goes: a web address or an email address. |
| `accent_color` | Hex colour for buttons and links; your brand colour when empty. |
| `font` | `system`, `sans` or `serif`. |
| `panel_origins` | Up to 10 exact site addresses (`https://www.yourbrand.com`) that may show the hub in a panel. Empty: your website. |

The response also has `url` (the hub's address), `embed_site` (the site where it opens in a panel, if any), `panel_origins_in_use`, `link_domain` and `available_types` (each object type with its fields, marked `sensitive`, `access_like` or `free_text`).

`GET /api/hub/preview` returns `{html}`: the hub as a made-up shopper would see it with your saved settings.

Turning the hub on or off and changing its settings are recorded in the audit log.
