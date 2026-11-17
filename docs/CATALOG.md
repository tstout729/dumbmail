# Product catalog

DumbMail keeps a catalog of your products so emails can show the right products automatically, and so it can tell people when something is back in stock or cheaper.

## Getting products in

There are five ways in. They all save products by **your store's id** (`external_id`): saving the same id again updates the product. Fields you leave out keep their saved value.

1. **From your site, with no setup.** The website snippet and the Shopify checkout pixel already send Viewed Product, Added to Cart, Started Checkout and Placed Order with product details. DumbMail reads those events and adds the products it sees, as long as you have no feed (turn this off with `learn_from_events`). Anyone can send events from a browser, so a product is only learned when its page is on your own site: your website (Settings → Account) or a verified sending domain, or a subdomain of either. Products from a feed, the API or a CSV are never changed by events. Product activity (who viewed, carted and bought what) is always read from events, whatever the source of the product.
2. **A feed**, fetched on a schedule (every 1 to 168 hours, default 6):
   - `json`: a JSON list of products, or an object holding one (`products`, `items`, `data`, `entries` or `results`; set `items_path` for anything else). Shopify's public `https://your-store.com/products.json` works as is.
   - `google_xml`: a Google Shopping / Merchant Center feed, RSS 2.0 or Atom with the `g:` namespace. Items sharing `g:item_group_id` become one product with variants. `g:sale_price` becomes the price and `g:price` the compare-at price. `g:product_type` becomes categories and `g:custom_label_0`–`4` become tags.
   - Feeds are fetched conditionally (ETag / Last-Modified), so an unchanged feed costs one small request. They must be public `http(s)` addresses (no private or local networks), at most 25 MB and 50,000 products. Products that leave a feed are unpublished (set `unpublish_missing: false` to keep them). Each feed shows its last sync, counts and errors.
3. **CSV upload**: a header row, then one row per product. Columns: `id`, `title`, `description`, `url`, `image`, `price`, `compare_at_price`, `currency`, `inventory`, `available`, `categories`, `tags`, `brand`, `vendor`, `published`. Add `variant_id` (and `variant_title`, `sku`) to make rows that share an `id` into variants. Other columns become custom fields. At most 50,000 rows and 20 MB.
4. **REST API**: `POST /api/catalog/products` with one product, a list, or `{"products": [...]}` (at most 1,000 per call).
5. **MCP**: `upsert_products` and `import_products_csv`.

### Field map

A feed's `field_map` says where each of our fields is in your feed, when the defaults don't find it. JSON uses dot paths (`data.items`, `pricing.current`); XML uses element names (`g:custom_label_1`, `title`). Keys: `items_path` (JSON only), `external_id`, `title`, `description`, `url`, `image_url`, `images`, `price`, `sale_price`, `compare_at_price`, `currency`, `available`, `inventory`, `categories`, `tags`, `brand`, `vendor`, `published`, `group_id` (XML variants), `variant_title`, `sku`, and `custom.<name>` for anything else.

```json
{"items_path": "data.catalog", "external_id": "code", "title": "label", "price": "cost", "custom.material": "specs.material"}
```

### A product

```json
{"external_id": "TEE-1", "title": "Organic tee", "url": "https://shop.example.com/tee", "image_url": "https://cdn.example.com/tee.jpg",
 "price": 24, "compare_at_price": 30, "currency": "USD", "categories": ["Shirts"], "tags": ["organic"], "brand": "Acme",
 "variants": [{"external_id": "TEE-1-S", "title": "Small", "sku": "T1S", "inventory": 0},
              {"external_id": "TEE-1-M", "title": "Medium", "sku": "T1M", "inventory": 12}]}
```

- `variants` is the full list: sending it replaces the saved variants. Leave it out to keep them.
- In stock: a product with variants is in stock when any variant is. A variant (or a product without variants) is in stock when `available` isn't false and `inventory` is more than 0 or not tracked.
- The product's price is its own `price`, else its cheapest variant's.
- Every change to the price or stock is kept in the product's history.
- `compare_at_price: 0` clears a sale. Unpublished products (`published: false`) never show in emails.

## Products in emails

Add a `products` block to an email's `blocks`. It's filled for each person when the email sends.

```json
{"type": "products", "source": "event_items", "count": 3, "columns": 3,
 "show_price": true, "show_compare_at": true, "show_button": true, "button_text": "Shop now",
 "fallback": "best_sellers", "hide_out_of_stock": true}
```

| `source` | Shows |
| --- | --- |
| `handpicked` | The products in `product_ids` (your ids), in that order. |
| `collection` | Products with a `category`, `tag` and/or `brand`, newest first. |
| `best_sellers` | Bought by the most people in the last `days` days (default 30). |
| `newest` | Most recently published. |
| `on_sale` | Biggest discount (compare-at price above price) first. |
| `event_items` | The triggering event's products: the cart for Added to Cart and Started Checkout, the viewed product for Viewed Product, the order's items for Placed Order, the product for Back in Stock and Price Dropped. Current prices and images come from the catalog when it has the product; for a product it doesn't have, the link is kept only if it's on your own site. |
| `recently_viewed` | What this person viewed, latest first. |
| `recommended` | Picks for this person (see Recommendations). |

- `count` 1–12 (default 4), `columns` 1–4 (default 2; they stack on phones).
- `fallback`: another source to use when this one has nothing (e.g. a campaign has no triggering event, or someone has no views yet). With nothing to show, the block is left out.
- `hide_out_of_stock` defaults to true.
- The preview shows the block for the profile you pick. With `event_items`, a preview (which has no event) uses that person's latest cart, checkout, order or product view.

### In templates (SMS and custom HTML too)

- `{{ catalog.products["TEE-1"].title }}`: any product by your id. Write the id literally so DumbMail knows to load it.
- `catalog.best_sellers`, `catalog.newest`, `catalog.on_sale`: lists of 8.
- `recommendations`: 8 picks for this person (best sellers when there's no person).

Each product has `id`, `title`, `url`, `image_url`, `price`, `compare_at_price`, `currency`, `price_text` ("$24.00"), `compare_at_text` (only when on sale), `on_sale`, `in_stock`, `categories`, `tags`, `brand` and `description` (plain text, up to 300 characters).

```
{% for p in recommendations[:2] %}{{ p.title }} {{ p.price_text }} {{ p.url }}
{% endfor %}
```

These only load when a template uses them, and lists that are the same for everyone are cached for a minute, so big sends stay fast.

## Recommendations

Computed from what people did, not a trained model:

- **Bought together**: how many people ordered both products in the same order.
- **Viewed together**: how many people viewed both in the last 90 days.
- **For a person**: products bought together with what they bought or carted, and viewed together with what they viewed; then popular products in the categories they browse most; leaving out anything they've bought; then best sellers to fill. Only published, in-stock products.

Bought-together and viewed-together counts are cached per product for six hours. Use `get_recommendations` with a `profile_id` or a `product_id`, or `GET /api/catalog/recommendations`.

## Back in stock

1. Add the widget to your product pages, after the DumbMail snippet:
   `<script src="https://dumbmail.ai/client/back-in-stock.js?k=YOUR_PUBLIC_KEY" async></script>` (the Products → Alerts page has yours).
2. On a sold-out product page it shows "Notify me when it's back" and an email box. It finds the product from `window.dumbmailBackInStock = {productId, variantId}`, a `<div data-dumbmail-back-in-stock data-product-id="..." data-variant-id="...">` placeholder (where it then appears), Shopify's product data, the page's structured data, or the page address. Without a placeholder it appears after the add-to-cart form. Style it with the CSS variables `--dm-bis-color`, `--dm-bis-text` and `--dm-bis-radius` on `.dm-bis`, or set `widget` in the settings.
3. A request records a **Subscribed to Back in Stock** event. It never changes the person's email consent.
4. When a feed, CSV or API update takes the product or variant from out of stock to in stock, DumbMail records a **Back in Stock** event for each person waiting, oldest request first, in batches of 200, up to `back_in_stock_limit` people per restock (empty = everyone). A request for the whole product is answered by any variant coming back. Each request is answered once; people ask again to be told again.
5. The **Back in stock** starter flow emails them. Normal consent rules apply.

The public endpoints, for custom widgets: `GET /client/products/stock?k=KEY&product_id=...&variant_id=...` and `POST /client/back-in-stock?k=KEY` with `{"email", "product_id", "variant_id"}`. A person can wait on at most 25 products at once.

## Price drops

When a feed, CSV or API update lowers a product's price by at least `price_drop_percent` percent and/or `price_drop_amount` (with both set, a drop must meet both), DumbMail records a **Price Dropped** event for each person who viewed, carted or started checkout with it in the last `price_drop_lookback_days` days (default 14) and didn't order it in that time, at most once per product per person every `price_drop_cooldown_days` days (default 30). The event has the product's fields plus `old_price`, `new_price`, `old_price_text`, `new_price_text`, `drop_amount` and `drop_percent`. The **Price drop** starter flow emails them. Prices learned from site events never trigger alerts.

## Segments

`{"type": "product", "action": "ordered" | "viewed" | "carted" | "checkout", "product_id": "TEE-1" | "category": "Shirts" | "tag": "organic" | "brand": "Acme", "op": "at_least" | "at_most" | "exactly" | "zero", "count": 2, "within_days": 90}`. Counts events (one order with two matching products counts once). `since` and `until` (`YYYY-MM-DD`) work too. Leave out the product, category, tag and brand to match any product.

## Settings

`get_catalog_settings` / `update_catalog_settings` (or `GET`/`PATCH /api/catalog/settings`):

- `learn_from_events` (default true)
- `back_in_stock_enabled` (default true), `back_in_stock_limit`
- `price_drop_enabled` (default true), `price_drop_percent` (default 10), `price_drop_amount`, `price_drop_lookback_days`, `price_drop_cooldown_days`
- `widget`: `button_text`, `heading`, `success_message`, `color`, `text_color`

## Tools and routes

| MCP tool | REST |
| --- | --- |
| `list_products` | `GET /api/catalog/products?q=&category=&tag=&brand=&in_stock=&on_sale=&limit=&offset=` |
| `get_product` | `GET /api/catalog/products/{id or your id}` |
| `upsert_products` | `POST /api/catalog/products` |
| `import_products_csv` | `POST /api/catalog/products/import` with `{"csv": "..."}` |
| `delete_product` (needs `confirm: true`) | `DELETE /api/catalog/products/{id}` |
| `list_product_feeds` | `GET /api/catalog/feeds` |
| `create_product_feed` | `POST /api/catalog/feeds` |
| `update_product_feed` | `PATCH /api/catalog/feeds/{id}` |
| `sync_product_feed` | `POST /api/catalog/feeds/{id}/sync` |
| `delete_product_feed` (needs `confirm: true`) | `DELETE /api/catalog/feeds/{id}` |
| `get_recommendations` | `GET /api/catalog/recommendations?profile_id=` or `?product_id=` |
| `get_catalog_settings` / `update_catalog_settings` | `GET` / `PATCH /api/catalog/settings` |

Also `GET /api/catalog/summary` (counts) and `GET /api/catalog/facets` (categories, tags and brands in use).
