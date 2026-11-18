# Shopify

Connect a Shopify store and DumbMail hears about every order, refund, shipment, cancellation and checkout from Shopify's own servers, keeps customers and their marketing consent in step, brings in years of history, keeps the product catalog current, and can make a single-use Shopify discount code for each person.

One store per DumbMail account. A store connected to one account can't be connected to another.

## Connecting

Settings → Shopify: type the store's address (`your-store.myshopify.com`, or just `your-store`) and choose **Connect**. Only a member with `shopify.manage` (an admin or the owner) can connect a store. DumbMail gives a link that goes on to Shopify, where the merchant approves the app, and Shopify sends them back. The link is good for 15 minutes. It finishes only in the browser that opened it, signed in to DumbMail as the person who started it, who must still hold `shopify.manage`. So nobody can attach a store to someone else's account, or finish a teammate's install, by sending a link. (`connect_shopify` with an API key: the first member with `shopify.manage` who opens the link is the one who finishes it.)

Opening the app from the store's Shopify admin (or installing it from the App Store) lands on Settings → Shopify with the store filled in; sign in and choose Connect.

After connecting, DumbMail subscribes the store to its webhooks, saves the shop's name, currency and web address, installs the checkout pixel and saves the script address the theme embed uses (see [On the storefront](#on-the-storefront)). Then it starts the history import on its own.

If Shopify refuses any webhook, the store shows as connected but not receiving those topics (for example "not receiving refunds/create"), as a problem, not a healthy connection. DumbMail tries again every 10 minutes, and starts the history import once every webhook is in place.

Each connection is a new install. Work queued for an earlier install (webhooks, reads, imports) is dropped and can't write to the new one. An `app/uninstalled` webhook only disconnects the install it was sent for: one Shopify triggered before the current install began is ignored.

**Disconnect** removes DumbMail's webhooks, pixel and the theme embed's settings from the store and forgets its access token. If Shopify doesn't confirm removing any of them, the answer and Settings → Shopify say which; uninstalling the app in Shopify removes them. Profiles, events and products already brought in stay. Uninstalling the app in Shopify does the same as disconnecting, from Shopify's side.

MCP: `connect_shopify` (returns the link to open), `disconnect_shopify` (`confirm: true`), `get_shopify_status`.

REST: `GET /api/shopify`, `POST /api/shopify/connect {shop}`, `POST /api/shopify/disconnect {confirm}`.

## Events from Shopify's servers

Shopify signs each webhook with the app's secret; anything with a wrong signature is refused. Each delivery is stored once (Shopify's `X-Shopify-Webhook-Id`), acknowledged at once, and processed in the background, so a redelivery changes nothing. Order, refund and checkout bodies are kept only until they're processed; product, customer and inventory webhooks keep just the id, and DumbMail reads the item from Shopify.

| Event | When | Value |
|---|---|---|
| Placed Order | an order is created (or first seen) | the order total less refunds; 0 once cancelled |
| Ordered Product | one per line item | the line total less its refunds; 0 once cancelled |
| Fulfilled Order | the whole order has shipped | the order total |
| Fulfilled Partial Order | each shipment of a partly shipped order | |
| Cancelled Order | the order is cancelled | the order total |
| Refunded Order | each refund | the amount refunded |
| Started Checkout | a checkout has the buyer's email or phone and isn't finished | the checkout total |

**Amounts** are in the shop's currency (Shopify's shop-money fields), exactly as Shopify gives them, with no rounding; `currency` is the shop's. A checkout the buyer paid in another currency is recorded without a value.

**Who it's for.** An order or checkout goes to the profile with the event's own email (or phone), found or made. If the Shopify customer was linked to a different profile, the link moves; nothing about the order goes to an address the buyer no longer uses. An order or checkout with no email or phone is recorded for the linked customer's profile with `$no_contact: true`, and starts no flow. Events keep `shopify_customer_id`, `order_id` and `shopify_shop`.

Properties use the same names as the website snippet and checkout pixel: `items` (each with `name`, `product_id`, `variant_id`, `variant`, `sku`, `price`, `quantity`, `image_url`, `url`, `brand`, `categories`), `item_names`, `item_count`, `total`, `currency`, `discount_codes`, `order_id`, `order_name` (#1001), `order_number`, and for checkouts `checkout_url`. Every event has `tracked_by: "shopify"`.

**One order, one event.** The snippet and pixel send `Placed Order` with `unique_id` `order:<id>` and `Started Checkout` with `checkout:<token>`; Shopify's events use the same ids. If a browser's copy (recorded before the store was connected) is there first, the trusted event replaces it as a fresh row with only Shopify's data (nothing of the browser's is kept), so flows, catalog learning and coupon redemption see it as new; a person already in a flow from the browser's copy isn't started again. The history import uses the same ids too. Revenue is never counted twice.

**Refunds and cancellations.** Revenue everywhere (the overview, reports, attribution, cohorts, segment totals and predictions) adds up `Placed Order` values. So rather than add negative events, DumbMail keeps each `Placed Order` value equal to what the store kept: the total less what was refunded, and 0 once the order is cancelled. `Ordered Product` values follow their line's refunds.

What was refunded is read from Shopify's Admin API for the whole order whenever a refund webhook or an order webhook mentions refunds, and after the history import. Only refund transactions that succeeded count: a pending or failed refund takes nothing off. Each read replaces what was known, so a later, fuller read corrects an earlier one, and a read older than the last one applied changes nothing. The order's value and its lines' values change together. An order webhook older than the one already applied (by Shopify's `updated_at`) changes nothing. The event shows `refunds`, `refunded_total` and `cancelled`. A cancelled order still counts as an order in order counts.

**Flows.** Events that happen now start flows. Events that reach DumbMail more than a day after they happened (an old order edited, or anything from the history import) are recorded as history: no flow starts, nothing is sent or billed.

**Trusted only from Shopify.** When a store connects, the app's metrics (`Placed Order`, `Started Checkout`, `Ordered Product`, `Fulfilled Order`, `Fulfilled Partial Order`, `Cancelled Order`, `Refunded Order`) become server-only for that account: the public `/client/events` endpoint refuses them, so the snippet and pixel can't add or change orders any more, and flows ignore browser copies recorded earlier. They stay server-only after a disconnect (the safer choice; ask support to change it). Accounts without the app keep sending these names from their sites as before.

Test orders (`test: true`) are ignored.

## Customers and consent

Shopify customers become profiles, matched by email, then phone, with Shopify's customer id kept (`shopify_customer_id`). A profile made here from the store's data (a customer, an order or a checkout) is held back from marketing (`marketing_hold`) until the person subscribes: campaigns skip them, and only flows marked as service messages (`settings.service_messages`: order and shipping updates) email them, at the address the store supplied (`email_source` is `integration:shopify:<store>`). Shopify's own subscribed state clears the hold. A profile that already existed is never held. Mark your order and shipping flows as service messages; leave marketing flows (winback, review asks) unmarked. Names, `city`, `region`, `country` (ISO code), `zip`, `shopify_tags`, `shopify_order_count`, `shopify_total_spent` and `shopify_currency` are saved as properties.

Consent comes only from Shopify's own consent fields, for the exact address or number Shopify reports it for:

- Email marketing **subscribed** in Shopify: subscribed here (unless the address unsubscribed here or is suppressed: those always win).
- **Unsubscribed** in Shopify: unsubscribed here.
- **Invalid** or **redacted**: never left subscribed (a subscribed or pending address becomes unsubscribed).
- Not subscribed or pending: nothing changes. A new profile starts as never subscribed, and Shopify's state is saved in `shopify_email_marketing`.
- SMS works the same from the SMS consent of that phone number. A number that changes in Shopify gets its own profile (or the profile that has it) with its own consent.
- Shopify's consent applies only when its time is newer than the last one applied for that address or number. A late webhook or an older import snapshot never undoes a newer choice, and an older customer snapshot never changes the link.
- Orders and checkouts never change anyone's consent.
- When a customer's email changes in Shopify, the new address gets only the consent Shopify reports for it (a different profile, or a profile that had no email); the old address keeps its own. Nothing carries over.

An unsubscribe in DumbMail (from an email's link, the preference page, a reply STOP, the API) is written back to Shopify's email or SMS consent within a minute or two, for the customer who has that address or number in Shopify. It counts as done only when Shopify accepts it. If Shopify refuses, DumbMail tries again later (from a minute, doubling, up to a day) and Settings → Shopify shows it. One in Shopify arrives here by webhook.

## History

Connecting starts it. `start_shopify_import` (`confirm: true`), or **Import history** in Settings → Shopify, runs it again. It reads every customer, every product, and orders from the last 3 years (`order_days` changes that), with their line items, refunds and shipments, using Shopify's bulk operations. It runs in the background in small steps; pause, resume and cancel it any time (`pause_shopify_import`, `resume_shopify_import`, `cancel_shopify_import`), and follow it with `get_shopify_import`. Running it again adds nothing twice.

Imported orders are history: they don't start flows, send anything, cost anything or count as fresh activity. Imported products queue no back-in-stock or price-drop alerts. Results are read with byte ranges; a server answering with the wrong range stops the import with a plain error, and one that ignores ranges is read by skipping ahead, up to 64 MB. While a page of history is being written, segments take in new members quietly (the same `absorb_entries` mechanism as the Klaviyo import): segment-triggered flows don't start for people history moved in. The window closes as soon as the import pauses, finishes, fails or is cancelled; each segment is refreshed once more quietly and then triggers as usual. A real order that arrives while an import is paused or between pages starts segment flows normally. Imported events go through the shared history path: no flows, no sends, no billing.

Orders older than 60 days need Shopify's `read_all_orders` permission (see below); without it Shopify returns only the last 60 days.

## Products

Products, variants, prices, compare-at prices, stock, images, collections (as categories, with the product type), tags, vendor and status go into the catalog (source "Shopify"), where they count as verified. Product and inventory webhooks keep them current (many updates to the same product are read once), and every product is read again once a day (`resync_shopify_products` does it now). That's what makes back-in-stock and price-drop alerts follow the store's real stock and prices. A product deleted in Shopify is unpublished here; drafts and archived products are unpublished. After a full read of the store's products, its products that weren't in it are unpublished. A full read never overwrites a product changed or deleted since its snapshot was taken. The store's first full load queues no alerts; later reads do (that's how a missed webhook's price drop is caught).

## Discount codes

A coupon (Content → Coupons) can get its codes from the store: link it with `set_shopify_coupon` (or `PUT /api/shopify/coupons/{coupon_id}`) and a template: `title`, `value` (`{"percentage": 10}` or `{"fixed_amount": 5}`), `minimum_subtotal`, `collection_ids`, `prefix`, `combines_with_other_discounts`, and optionally `expires_after_days` (otherwise the coupon's own expiry). When a message that shows the coupon is about to go and the coupon has no free code for that person, the coupons module asks the store through its one hook (`give_code`'s `CreateInStore`): the person gets a real, single-use Shopify code (one use, once per customer), saved as their code in the coupon. If Shopify is rate-limiting or down, that message waits 30 seconds and tries again. Sign-up forms hand out only codes already in the coupon. `remove_shopify_coupon` unlinks it; codes already given stay theirs. Each code belongs to one store, one template, one person and one grant: a retry after a crash makes the same code, not a second one, and accepts it only if the store's discount under that code has the template's terms. Reconnecting another store, reusing a template or the same grant for someone else never hands out another code. The template's stacking setting applies to order, product and shipping discounts alike. A code counts as used when an order from that store's servers carries it.

## On the storefront

The app can put DumbMail on the store without pasting code:

- **Theme app embed** (`connectors/shopify/extensions/dumbmail-embed`): loads the website snippet on every page, once Shopify's Customer Privacy API says the visitor allows analytics. Recognising visitors by the email they type also needs marketing to be allowed; without it the snippet loads with email capture off. Consent given later loads it then. Consent withdrawn after it loaded calls the snippet's `dumbmail.stop()`: the page sends no more events, pings or identification and writes no more tracking data to the browser, and events still waiting to be sent (for a visitor not yet recognised) are deleted along with who the snippet thought the visitor was. A sign-up form the visitor submits still goes through. Disconnecting clears its settings. DumbMail saves the snippet's address in the app's settings in the store when it connects; the merchant turns the embed on once in the theme editor (Settings → Shopify has a link that opens it ready to save).
- **Web pixel** (`connectors/shopify/extensions/dumbmail-pixel`): reports Started Checkout and Placed Order from checkout pages and tells the storefront who checked out. DumbMail installs it when it connects. An event with no order id or checkout token is not sent (it couldn't be matched to Shopify's own copy).

Both need the app's extensions deployed with the Shopify CLI (see `connectors/shopify/README.md`). Without them, the snippet and the custom checkout pixel from Settings → Website still work as before.

## Privacy webhooks

They act on Shopify's own ids: the customer, the orders and the store named in the request, never on whoever shares an email address or phone number. DumbMail records what came from which store and customer as it arrives (the customer link, every profile a customer touched, profiles DumbMail made, and the store, customer and order ids on every event), and keeps a record of past installs, so a request still finds the right account after the store is disconnected or replaced. The store a request is about is the `shop_domain` in its signed body, and must match the header.

- **Customer data request:** DumbMail gathers the profiles linked to that Shopify customer, their events and messages, and the store's events for the orders asked about, into an export. It emails the account's Owner that it's ready, and lists it in Settings → Shopify → Data requests by Shopify customer id (`get_shopify_data_request` returns it and records the download). Exports are kept 90 days.
- **Customer redact:** deletes that customer's Shopify data: the link, the order and checkout events for that customer and for the orders named, the Shopify properties on their profiles, the consent Shopify reported, and any export made for them. A profile DumbMail made for them is deleted when nothing else is on it (no other events, no list membership); otherwise it stays without the Shopify data.
- **Shop redact** (Shopify sends it 48 hours after the app is uninstalled): the same for every customer of the store, plus its products in the catalog, queued work, stored deliveries, imports, discount code records, exports and the connection. Ignored if the store has been connected again since.

An erasure is recorded first, so a running import or a late webhook skips what was erased instead of bringing it back. A request that fails is tried again with growing waits without holding up the others; one still failing after 7 days is logged as an error and shown in Settings → Shopify (Shopify expects an answer within 30 days). Webhook bodies are deleted once processed; one left unprocessed for 30 days is deleted and counted in status.

## Health

Settings → Shopify shows "Orders are arriving from Shopify" when orders came from Shopify's servers in the last week, and "Only the browser snippet is reporting orders" when the store isn't connected but the snippet is reporting them. It shows "Connected, but not receiving …" when Shopify refused a webhook. It also shows the last webhook, import progress, product sync and problems in plain words: unsubscribes Shopify hasn't accepted yet, privacy requests failing for over 7 days, clean-up a disconnect couldn't finish, and webhook bodies deleted unprocessed.

## Setting it up (operator)

The app's API version is `2026-10` (`API_VERSION` in `src/features/shopify/mod.rs`), supported by Shopify until **October 16, 2027**. Move to a newer stable version before then: change the constant and `api_version` in `connectors/shopify/shopify.app.toml`, and check Shopify's changelog for changes to the fields used here.

In the Shopify Partner Dashboard (Dev Dashboard), or with the Shopify CLI from `connectors/shopify`:

1. **Create an app.** A **public app** (listed or unlisted) to install on any store; or a **custom app** made in the Partner Dashboard for one store (distribution: custom), which needs no App Store review.
2. **URLs** (with PUBLIC_URL, e.g. `https://app.dumbmail.ai`):
   - App URL: `{PUBLIC_URL}/api/shopify/app`
   - Allowed redirection URL: `{PUBLIC_URL}/api/shopify/callback`
   - Embedded in Shopify admin: **off** (DumbMail is its own dashboard).
3. **Scopes:** `write_customers`, `read_orders`, `read_all_orders`, `read_products`, `read_inventory`, `write_discounts`, `write_pixels`, `read_customer_events`. `read_all_orders` (orders older than 60 days) has to be requested and approved in the Partner Dashboard.
4. **Protected customer data:** request access to customer data, with the name, email, phone and address fields (Partner Dashboard → API access → Protected customer data), and fill in the data protection details. Without it Shopify leaves those fields out.
5. **Privacy webhooks:** `customers/data_request`, `customers/redact` and `shop/redact`, all to `{PUBLIC_URL}/webhooks/shopify` (in `shopify.app.toml` as `compliance_topics`, or in the Partner Dashboard). Every other topic DumbMail subscribes each store to itself; don't add them in the app's configuration too, or each event arrives twice.
6. **Bulk operations:** apps made in the Partner Dashboard may need bulk queries enabled for the app.
7. **Server:** set `SHOPIFY_API_KEY` (client ID) and `SHOPIFY_API_SECRET` (client secret). Without both, Settings → Shopify says Shopify isn't available on this server.
8. **Extensions:** deploy them with `shopify app deploy` from `connectors/shopify` (see its README).
9. **App review (public apps):** Shopify checks that the privacy webhooks answer correctly, that a bad webhook signature gets 401, that install and uninstall work, and asks for a test store login, screencast and privacy policy URL. A custom app for one store skips review.

New public apps get expiring access tokens; DumbMail refreshes them on its own. If the refresh token stops working (after 90 days without use, say), Settings → Shopify asks to connect the store again.

## Reference

MCP tools: `get_shopify_status`, `connect_shopify`, `disconnect_shopify`, `start_shopify_import`, `get_shopify_import`, `pause_shopify_import`, `resume_shopify_import`, `cancel_shopify_import`, `resync_shopify_products`, `get_shopify_data_request`, `set_shopify_coupon`, `remove_shopify_coupon`.

**Permissions.** Seeing the connection, its status and import progress needs `account.read` (every role). Connecting, disconnecting, starting, pausing, resuming or cancelling the import, syncing products, linking coupons and downloading a privacy export need `shopify.manage` (admins and the owner); the dashboard hides those controls from other roles. Connecting, disconnecting, the import actions, coupon links and privacy exports are recorded in the audit log, without tokens or customer data. Shopify's own requests (the app URL, the OAuth callback, webhooks) are public and checked by Shopify's signature; the install link and callback check the signed-in member themselves.

**Background work.** Webhook processing, write-backs, imports, product reconciles, privacy requests, segment re-baselines and clean-up all run as jobs in the shared background lane, never in the lane sending uses; one worker at a time per store.

REST (signed-in session or API key):

| | |
|---|---|
| `GET /api/shopify` | status (as `get_shopify_status`) |
| `POST /api/shopify/connect` | `{shop}` → `{url}` to open |
| `POST /api/shopify/disconnect` | `{confirm: true}` |
| `GET /api/shopify/import` | latest import and product sync |
| `POST /api/shopify/import` | `{confirm: true, order_days?}` |
| `POST /api/shopify/import/pause` · `/resume` · `/cancel` | |
| `POST /api/shopify/products/resync` | |
| `GET /api/shopify/data-requests/{id}` | the export |
| `PUT /api/shopify/coupons/{coupon_id}` · `DELETE` | link a coupon to the store, or unlink it |

Public (signed by Shopify): `GET /api/shopify/app`, `GET /api/shopify/callback`, `POST /webhooks/shopify`. `GET /api/shopify/install?state=…` is the install link `connect` returns; it needs a signed-in member with `shopify.manage`.
