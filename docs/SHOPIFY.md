# Shopify

Connect a Shopify store and DumbMail hears about every order, refund, shipment, cancellation and checkout from Shopify's own servers, keeps customers and their marketing consent in step, brings in years of history, keeps the product catalog current, and can make a single-use Shopify discount code for each person.

One store per DumbMail account. A store connected to one account can't be connected to another.

## Connecting

Settings → Shopify: type the store's address (`your-store.myshopify.com`, or just `your-store`) and choose **Connect**. Shopify asks the merchant to approve the app, then sends them back. The link is good for 15 minutes and works only in a browser signed in to the DumbMail account that asked for it, so nobody can attach a store to someone else's account by sending a link.

Opening the app from the store's Shopify admin (or installing it from the App Store) lands on Settings → Shopify with the store filled in; sign in and choose Connect.

After connecting, DumbMail subscribes the store to its webhooks, saves the shop's name, currency and web address, installs the checkout pixel and saves the script address the theme embed uses (see [On the storefront](#on-the-storefront)). Then start the history import.

**Disconnect** removes DumbMail's webhooks and pixel from the store and forgets its access token. Profiles, events and products already brought in stay. Uninstalling the app in Shopify does the same from Shopify's side.

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

Properties use the same names as the website snippet and checkout pixel: `items` (each with `name`, `product_id`, `variant_id`, `variant`, `sku`, `price`, `quantity`, `image_url`, `url`, `brand`, `categories`), `item_names`, `item_count`, `total`, `currency`, `discount_codes`, `order_id`, `order_name` (#1001), `order_number`, and for checkouts `checkout_url`. Every event has `tracked_by: "shopify"`.

**One order, one event.** The snippet and pixel send `Placed Order` with `unique_id` `order:<id>` and `Started Checkout` with `checkout:<token>`; Shopify's events use the same ids. If the browser's copy arrives first, Shopify's facts replace it in place and it becomes a server event (trusted); if Shopify's arrives first, the browser's is dropped as a duplicate. The history import uses the same ids too. Revenue is never counted twice.

**Refunds and cancellations.** Revenue everywhere (the overview, reports, attribution, cohorts, segment totals and predictions) adds up `Placed Order` values. So rather than add negative events, DumbMail keeps each `Placed Order` value equal to what the store kept: the total less every refund, and 0 once the order is cancelled. `Ordered Product` values follow their line's refunds. The refunds already counted are kept on the event (`refunds`, `refunded_total`, `cancelled`), so the same refund never counts twice. A cancelled order still counts as an order in order counts.

**Flows.** Events that happen now start flows. Events that reach DumbMail more than a day after they happened (an old order edited, or anything from the history import) are recorded as history: no flow starts, nothing is sent or billed.

**Trusted only from Shopify.** `Ordered Product`, `Fulfilled Order`, `Fulfilled Partial Order`, `Cancelled Order` and `Refunded Order` can't be sent from a browser (the public `/client/events` endpoint refuses them). `Placed Order` and `Started Checkout` can still come from the snippet, so stores without the app keep working; the browser's copies stay marked as from the browser until Shopify confirms them.

Test orders (`test: true`) are ignored.

## Customers and consent

Shopify customers become profiles, matched by email, then phone, with Shopify's customer id kept (`shopify_customer_id`). Names, `city`, `region`, `country` (ISO code), `zip`, `shopify_tags`, `shopify_order_count`, `shopify_total_spent` and `shopify_currency` are saved as properties.

Consent comes only from Shopify's own consent fields, for the exact address or number Shopify reports it for:

- Email marketing **subscribed** in Shopify: subscribed here (unless the address unsubscribed here or is suppressed: those always win).
- **Unsubscribed** in Shopify: unsubscribed here.
- Anything else (not subscribed, pending, invalid, redacted): nothing changes. A new profile starts as never subscribed, and Shopify's state is saved in `shopify_email_marketing`.
- SMS works the same from the SMS consent of that phone number.
- Orders and checkouts never change anyone's consent.
- When a customer's email changes in Shopify, the new address gets only the consent Shopify reports for it (a different profile, or a profile that had no email); the old address keeps its own. Nothing carries over.

An unsubscribe in DumbMail (from an email's link, the preference page, a reply STOP, the API) is written back to Shopify's email or SMS consent within a minute or two. One in Shopify arrives here by webhook.

## History

`start_shopify_import` (`confirm: true`), or **Import history** in Settings → Shopify, reads every customer, every product, and orders from the last 3 years (`order_days` changes that), with their line items, refunds and shipments, using Shopify's bulk operations. It runs in the background in small steps; pause, resume and cancel it any time (`pause_shopify_import`, `resume_shopify_import`, `cancel_shopify_import`), and follow it with `get_shopify_import`. Running it again adds nothing twice.

Imported orders are history: they don't start flows, send anything, cost anything or count as fresh activity. While an import runs, and on the first segment refresh after it ends, segment-triggered flows don't start for people who entered the segment; that refresh just records who's in.

Orders older than 60 days need Shopify's `read_all_orders` permission (see below); without it Shopify returns only the last 60 days.

## Products

Products, variants, prices, compare-at prices, stock, images, collections (as categories, with the product type), tags, vendor and status go into the catalog (source "Shopify"), where they count as verified. Product and inventory webhooks keep them current (many updates to the same product are read once), and every product is read again once a day (`resync_shopify_products` does it now). That's what makes back-in-stock and price-drop alerts follow the store's real stock and prices. A product deleted in Shopify is unpublished here; drafts and archived products are unpublished.

## Discount codes

Coupons can be made in Shopify on demand: each person gets a real, single-use code (one use, once per customer) from a template: percentage or fixed amount, minimum subtotal, expiry, and everything or chosen collections. Codes are made with the Admin API ahead of a campaign send in small batches (Shopify rate-limits them), never one call inside the send. A retry after a crash makes the same code, not a second one. A code counts as used when an order from Shopify's servers carries it.

## On the storefront

The app can put DumbMail on the store without pasting code:

- **Theme app embed** (`connectors/shopify/extensions/dumbmail-embed`): loads the website snippet on every page. DumbMail saves the snippet's address in the app's settings in the store when it connects; the merchant turns the embed on once in the theme editor (Settings → Shopify has a link that opens it ready to save).
- **Web pixel** (`connectors/shopify/extensions/dumbmail-pixel`): reports Started Checkout and Placed Order from checkout pages and tells the storefront who checked out. DumbMail installs it when it connects.

Both need the app's extensions deployed with the Shopify CLI (see `connectors/shopify/README.md`). Without them, the snippet and the custom checkout pixel from Settings → Website still work as before.

## Privacy webhooks

- **Customer data request:** DumbMail gathers everything it holds on that customer in this account (profiles matched by Shopify customer, email or phone, with their events and messages) into an export, emails the account owner that it's ready, and lists it in Settings → Shopify → Data requests (`get_shopify_data_request`). Exports are kept 90 days.
- **Customer redact:** deletes that customer's profiles, with their events and messages.
- **Shop redact** (Shopify sends it 48 hours after the app is uninstalled): deletes what the connection holds for the store: tokens, customer links, Shopify's consent states, webhook records, imports, discount code records and data request exports. The account's own profiles, events and products stay; the account owner can delete them.

## Health

Settings → Shopify shows "Orders are arriving from Shopify" when orders came from Shopify's servers in the last week, and "Only the browser snippet is reporting orders" when the store isn't connected but the snippet is reporting them. It also shows the last webhook, import progress, product sync and problems in plain words.

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

MCP tools: `get_shopify_status`, `connect_shopify`, `disconnect_shopify`, `start_shopify_import`, `get_shopify_import`, `pause_shopify_import`, `resume_shopify_import`, `cancel_shopify_import`, `resync_shopify_products`, `get_shopify_data_request`.

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

Public (signed by Shopify): `GET /api/shopify/app`, `GET /api/shopify/callback`, `POST /webhooks/shopify`.
