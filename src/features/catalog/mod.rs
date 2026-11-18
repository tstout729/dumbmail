//! Product catalog: products and variants, feeds, products in emails, recommendations, and
//! back-in-stock and price-drop alerts. See docs/CATALOG.md.
//!
//! - `store`: products and variants; [`store::upsert_products`] is the one way authoritative data gets in.
//! - `feeds`: JSON and Google Shopping XML feeds on a schedule, and CSV import.
//! - `learn`: reads product activity (and, with no feed, the products themselves) from site events.
//! - `recs`: best sellers, bought/viewed together, picks per person.
//! - `blocks`: fills `products` email blocks and the `catalog` / `recommendations` template variables.
//! - `alerts`: back-in-stock and price-drop events, and the on-site "Notify me" widget.
//! - `verify`: confirms products learned from browser events by reading their page on the store's site.

pub mod alerts;
pub mod api;
pub mod blocks;
pub mod feeds;
pub mod learn;
pub mod recs;
pub mod store;
pub mod verify;

use async_trait::async_trait;
use axum::Router;
use serde_json::{json, Value};
use uuid::Uuid;

use super::team::access::{rule, Access, Perm, RouteRule, ToolRule};
use super::{Feature, McpDoc};

use Perm::{CatalogDelete, CatalogWrite, ContentRead};

static ROUTE_RULES: &[RouteRule] = &[
    rule("GET", "/api/catalog/summary", ContentRead),
    rule("GET", "/api/catalog/facets", ContentRead),
    rule("GET", "/api/catalog/products", ContentRead),
    rule("POST", "/api/catalog/products", CatalogWrite),
    rule("POST", "/api/catalog/products/import", CatalogWrite),
    rule("GET", "/api/catalog/products/{key}", ContentRead),
    rule("DELETE", "/api/catalog/products/{key}", CatalogDelete),
    // Picks for a person also need profiles.read (checked in the handler).
    rule("GET", "/api/catalog/recommendations", ContentRead),
    rule("GET", "/api/catalog/feeds", ContentRead),
    rule("POST", "/api/catalog/feeds", CatalogWrite),
    rule("GET", "/api/catalog/feeds/{id}", ContentRead),
    rule("PATCH", "/api/catalog/feeds/{id}", CatalogWrite),
    rule("DELETE", "/api/catalog/feeds/{id}", CatalogDelete),
    rule("POST", "/api/catalog/feeds/{id}/sync", CatalogWrite),
    rule("GET", "/api/catalog/settings", ContentRead),
    rule("PATCH", "/api/catalog/settings", CatalogWrite),
];

static TOOL_RULES: &[ToolRule] = &[
    ToolRule { name: "list_products", perm: ContentRead },
    ToolRule { name: "get_product", perm: ContentRead },
    ToolRule { name: "upsert_products", perm: CatalogWrite },
    ToolRule { name: "import_products_csv", perm: CatalogWrite },
    ToolRule { name: "delete_product", perm: CatalogDelete },
    ToolRule { name: "list_product_feeds", perm: ContentRead },
    ToolRule { name: "create_product_feed", perm: CatalogWrite },
    ToolRule { name: "update_product_feed", perm: CatalogWrite },
    ToolRule { name: "sync_product_feed", perm: CatalogWrite },
    ToolRule { name: "delete_product_feed", perm: CatalogDelete },
    ToolRule { name: "get_recommendations", perm: ContentRead },
    ToolRule { name: "get_catalog_settings", perm: ContentRead },
    ToolRule { name: "update_catalog_settings", perm: CatalogWrite },
];
use crate::auth::Ctx;
use crate::error::{AppError, AppResult};
use crate::mcp::{from, obj, tool};
use crate::state::AppState;

pub use blocks::personalize;

pub struct Module;

const DOC: &str = include_str!("../../../docs/CATALOG.md");

fn product_schema() -> Value {
    let s = |d: &str| json!({"type": "string", "description": d});
    let list = |d: &str| json!({"type": "array", "items": {"type": "string"}, "description": d});
    json!({
        "type": "object",
        "properties": {
            "external_id": s("The store's id for the product (required). Saving the same id again updates it."),
            "title": s("Required for a new product."),
            "description": s("Plain text or HTML."),
            "url": s("Product page (http/https)."),
            "image_url": s("Main image (http/https)."),
            "images": list("More images."),
            "price": {"type": "number"},
            "compare_at_price": {"type": "number", "description": "The original price when on sale. 0 clears it."},
            "currency": s("3-letter code, e.g. USD."),
            "available": {"type": "boolean", "description": "Sellable. With variants, stock comes from the variants."},
            "inventory": {"type": "integer", "description": "Units left. Leave out if not tracked."},
            "categories": list("Categories or collections."),
            "tags": list("Tags."),
            "brand": s("Brand."),
            "vendor": s("Vendor."),
            "custom": {"type": "object", "description": "Any other fields (up to 16 KB)."},
            "published": {"type": "boolean", "description": "Unpublished products never show in emails."},
            "published_at": {"type": "string", "format": "date-time", "description": "When it launched (sorts \"newest\")."},
            "variants": {"type": "array", "description": "The full list of variants (replaces the saved ones). Leave out to keep them.", "items": {"type": "object", "properties": {
                "external_id": s("Variant id (required)."), "title": s("e.g. Small / Red"), "options": {"type": "object"}, "sku": s("SKU"),
                "price": {"type": "number"}, "compare_at_price": {"type": "number"}, "available": {"type": "boolean"}, "inventory": {"type": "integer"}, "image_url": s("Variant image.")
            }, "required": ["external_id"]}}
        },
        "required": ["external_id"]
    })
}

fn feed_props() -> Value {
    json!({
        "name": {"type": "string"},
        "url": {"type": "string", "description": "Public http(s) address of the feed."},
        "format": {"enum": ["json", "google_xml"], "description": "json (any JSON list of products, including Shopify's /products.json) or google_xml (Google Shopping RSS/Atom)."},
        "field_map": {"type": "object", "description": "Our field -> the feed's field (JSON: a dot path like data.items.0.name; XML: an element like g:custom_label_0). Keys: items_path, external_id, title, description, url, image_url, images, price, sale_price, compare_at_price, currency, available, inventory, categories, tags, brand, vendor, published, group_id, variant_title, sku, custom.<name>."},
        "interval_hours": {"type": "integer", "minimum": 1, "maximum": 168},
        "enabled": {"type": "boolean"},
        "unpublish_missing": {"type": "boolean", "description": "Unpublish this feed's products that leave the feed (default true)."}
    })
}

fn tools() -> Vec<Value> {
    vec![
        tool("list_products", "List products", "Search the product catalog by title, id, SKU or brand, with filters. Returns price, stock and where each product came from.",
            obj(json!({"q": {"type": "string"}, "category": {"type": "string"}, "tag": {"type": "string"}, "brand": {"type": "string"}, "in_stock": {"type": "boolean"},
                       "on_sale": {"type": "boolean"}, "published": {"type": "boolean"},
                       "verified": {"type": "boolean", "description": "false: products learned from site events that DumbMail hasn't confirmed from their page yet."}, "limit": {"type": "integer", "maximum": 200}, "offset": {"type": "integer"}}), &[]), true, false),
        tool("get_product", "Get a product", "One product by DumbMail id or the store's id: variants, price and stock history, recent activity and who's waiting for a restock.",
            obj(json!({"product_id": {"type": "string"}}), &["product_id"]), true, false),
        tool("upsert_products", "Add or update products", "Save up to 1000 products by the store's id. Fields left out keep their saved value. Price drops and restocks found here trigger the Price Dropped and Back in Stock events (see dumbmail://docs/catalog).",
            obj(json!({"products": {"type": "array", "items": product_schema(), "maxItems": 1000}}), &["products"]), false, false),
        tool("import_products_csv", "Import products from CSV", "Import CSV text with a header row (id, title, price, url, image, inventory, categories, tags, brand, ... ; rows sharing an id with a variant_id column become variants).",
            obj(json!({"csv": {"type": "string"}}), &["csv"]), false, false),
        tool("delete_product", "Delete a product", "Delete a product, its variants, history and the restock requests waiting on it. Without confirm=true it only describes what would happen.",
            obj(json!({"product_id": {"type": "string"}, "confirm": {"type": "boolean"}}), &["product_id"]), false, true),
        tool("list_product_feeds", "List product feeds", "Product feeds with their last sync, product counts and errors.", obj(json!({}), &[]), true, false),
        tool("create_product_feed", "Add a product feed", "Add a JSON or Google Shopping XML feed. It syncs now and then every interval_hours.", obj(feed_props(), &["url"]), false, false),
        tool("update_product_feed", "Change a product feed", "Change a feed's address, format, field map, schedule or turn it on or off.",
            obj({ let mut p = feed_props(); p["id"] = json!({"type": "string"}); p }, &["id"]), false, false),
        tool("sync_product_feed", "Sync a product feed now", "Fetch the feed again now (in the background) instead of waiting for its schedule.", obj(json!({"id": {"type": "string"}}), &["id"]), false, false),
        tool("delete_product_feed", "Delete a product feed", "Remove a feed. Its products stay in the catalog but stop updating. Without confirm=true it only describes what would happen.",
            obj(json!({"id": {"type": "string"}, "confirm": {"type": "boolean"}}), &["id"]), false, true),
        tool("get_recommendations", "Get product recommendations", "For a profile: their picks and recently viewed products. For a product: products bought together and viewed together. Computed from orders and views, not a trained model.",
            obj(json!({"profile_id": {"type": "string"}, "product_id": {"type": "string"}, "limit": {"type": "integer", "maximum": 24}}), &[]), true, false),
        tool("get_catalog_settings", "Get catalog and alert settings", "Back-in-stock and price-drop settings, learning products from site events, and the back-in-stock widget's install snippet.", obj(json!({}), &[]), true, false),
        tool("update_catalog_settings", "Change catalog and alert settings", "Turn back-in-stock or price-drop alerts on or off and set their limits, thresholds and wording.",
            obj(json!({
                "learn_from_events": {"type": "boolean", "description": "Add products seen in site events while there's no feed."},
                "back_in_stock_enabled": {"type": "boolean"},
                "back_in_stock_limit": {"type": ["integer", "null"], "description": "Most people told per restock, oldest request first. null = everyone."},
                "price_drop_enabled": {"type": "boolean"},
                "price_drop_percent": {"type": ["number", "null"], "description": "Smallest drop that counts, in percent."},
                "price_drop_amount": {"type": ["number", "null"], "description": "Smallest drop that counts, in the product's currency. With both set, a drop must meet both."},
                "price_drop_lookback_days": {"type": "integer", "description": "Tell people who viewed or carted it in this many days."},
                "price_drop_cooldown_days": {"type": "integer", "description": "Tell a person about the same product at most once in this many days."},
                "widget": {"type": "object", "description": "button_text, heading, success_message, color, text_color."}
            }), &[]), false, false),
    ]
}

fn id_arg(a: &Value, k: &str) -> AppResult<Uuid> {
    a[k].as_str().and_then(|s| s.parse().ok()).ok_or_else(|| AppError::bad(format!("{k} must be an id")))
}

fn feed_input(a: &Value) -> AppResult<feeds::FeedInput> {
    let mut v = a.clone();
    if let Some(o) = v.as_object_mut() {
        o.remove("id");
    }
    from(v)
}

async fn call(state: &AppState, ctx: &Ctx, name: &str, a: &Value) -> AppResult<Value> {
    let acct = ctx.account_id;
    match name {
        "list_products" => api::list_products(state, acct, &from(a.clone())?).await,
        "get_product" => api::product_detail(state, ctx, a["product_id"].as_str().unwrap_or("")).await,
        "upsert_products" => {
            let items = api::products_from(json!({"products": a["products"].clone()}))?;
            Ok(json!(store::upsert_products(&state.db, acct, &items, store::Origin::Api).await?))
        }
        "import_products_csv" => api::import_csv(state, acct, a["csv"].as_str().unwrap_or("")).await,
        "delete_product" => {
            let key = a["product_id"].as_str().unwrap_or("");
            if a["confirm"].as_bool() != Some(true) {
                let p = store::find(&state.db, acct, key).await?;
                let waiting: i64 = sqlx::query_scalar("SELECT count(*) FROM catalog_stock_subscriptions WHERE product_id = $1 AND notified_at IS NULL").bind(p.id).fetch_one(&state.db).await?;
                return Ok(json!({"dry_run": true, "product": store::card(&p),
                    "note": format!("Nothing deleted. This removes {:?} with its variants, history and {waiting} waiting restock request(s). If a feed still lists it, the next sync adds it back. Call again with confirm=true once the user approves.", p.title)}));
            }
            let p = store::delete(&state.db, acct, key).await?;
            crate::features::team::audit(state, ctx, "product.delete", Some(&p.id.to_string()), json!({"title": p.title})).await;
            Ok(json!({"ok": true, "deleted": p.id}))
        }
        "list_product_feeds" => Ok(json!({"data": feeds::list(&state.db, acct).await?})),
        "create_product_feed" => {
            let f = feeds::create(&state.db, acct, feed_input(a)?).await?;
            api::feed_changed(state, ctx, "product_feed.create", &f).await;
            Ok(json!(f))
        }
        "update_product_feed" => {
            let f = feeds::update(&state.db, acct, id_arg(a, "id")?, feed_input(a)?).await?;
            api::feed_changed(state, ctx, "product_feed.update", &f).await;
            Ok(json!(f))
        }
        "sync_product_feed" => api::sync_feed(state, acct, id_arg(a, "id")?).await,
        "delete_product_feed" => {
            let id = id_arg(a, "id")?;
            let f = feeds::get(&state.db, acct, id).await?;
            if a["confirm"].as_bool() != Some(true) {
                return Ok(json!({"dry_run": true, "note": format!("Nothing deleted. This removes the feed {:?}; its products stay but stop updating. Call again with confirm=true once the user approves.", f.name)}));
            }
            feeds::delete(&state.db, acct, id).await?;
            crate::features::team::audit(state, ctx, "product_feed.delete", Some(&id.to_string()), json!({"name": f.name})).await;
            Ok(json!({"ok": true}))
        }
        "get_recommendations" => {
            let q = api::RecsQuery {
                profile_id: a["profile_id"].as_str().map(|s| s.parse().map_err(|_| AppError::bad("profile_id must be an id"))).transpose()?,
                product_id: a["product_id"].as_str().map(String::from),
                limit: a["limit"].as_i64(),
            };
            api::recommendations_for(state, ctx, &q).await
        }
        "get_catalog_settings" => api::settings_json(state, acct).await,
        "update_catalog_settings" => {
            alerts::update_settings(&state.db, acct, from(a.clone())?).await?;
            crate::features::team::audit(state, ctx, "catalog.settings_change", None, json!({})).await;
            api::settings_json(state, acct).await
        }
        _ => Err(AppError::not_found("tool")),
    }
}

#[async_trait]
impl Feature for Module {
    fn name(&self) -> &'static str {
        "catalog"
    }

    fn routes(&self) -> Router<AppState> {
        api::routes()
    }

    /// Reading the catalog is for everyone who sees content; changing it for managers and up;
    /// deleting for admins and up. The widget, stock and confirmation endpoints are on the public
    /// list in `team::access::PUBLIC_ROUTES`.
    fn access(&self) -> Access {
        Access { routes: ROUTE_RULES, tools: TOOL_RULES }
    }

    fn mcp_tools(&self) -> Vec<Value> {
        tools()
    }

    async fn mcp_call(&self, state: &AppState, ctx: &Ctx, name: &str, args: &Value) -> Option<AppResult<Value>> {
        if !tools().iter().any(|t| t["name"] == name) {
            return None;
        }
        Some(call(state, ctx, name, args).await)
    }

    fn mcp_docs(&self) -> Vec<McpDoc> {
        vec![McpDoc {
            slug: "catalog",
            name: "Product catalog",
            description: "Products, feeds, product blocks in email, recommendations, back-in-stock and price-drop alerts, and product segment rules.",
            text: DOC,
        }]
    }

    async fn run_job(&self, state: &AppState, kind: &str, payload: &Value) -> Option<anyhow::Result<()>> {
        Some(match kind {
            "catalog_feed_sync" => feeds::run_sync(state, payload).await,
            "catalog_back_in_stock" => alerts::run_back_in_stock(state, payload).await,
            "catalog_price_drop" => alerts::run_price_drop(state, payload).await,
            "catalog_verify" => verify::run(state, payload).await,
            "catalog_learn" => match learn::run(state, payload["lag_secs"].as_i64().unwrap_or(5)).await {
                Ok(true) => crate::jobs::enqueue(&state.db, "catalog_learn", payload.clone(), None).await.map(|_| ()).map_err(Into::into),
                Ok(false) => Ok(()),
                Err(e) => Err(e),
            },
            _ => return None,
        })
    }

    async fn tick(&self, state: &AppState, tick: u64) -> anyhow::Result<()> {
        sqlx::query(
            "INSERT INTO jobs (kind, payload) SELECT 'catalog_learn', '{}'::jsonb
             WHERE NOT EXISTS (SELECT 1 FROM jobs WHERE kind = 'catalog_learn' AND run_at <> 'infinity')",
        )
        .execute(&state.db)
        .await?;
        if tick.is_multiple_of(3) {
            feeds::queue_due(&state.db).await?;
            verify::queue_due(&state.db).await?;
        }
        if tick.is_multiple_of(180) {
            sqlx::query("DELETE FROM catalog_price_drop_notices WHERE notified_at < now() - interval '400 days'").execute(&state.db).await?;
            // Back-in-stock requests nobody confirmed.
            sqlx::query("DELETE FROM catalog_stock_subscriptions WHERE confirmed_at IS NULL AND created_at < now() - make_interval(days => $1)")
                .bind(alerts::CONFIRM_DAYS)
                .execute(&state.db)
                .await?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tools_have_object_schemas_and_docs_name_them() {
        for t in tools() {
            assert_eq!(t["inputSchema"]["type"], "object", "{}", t["name"]);
            let name = t["name"].as_str().unwrap();
            assert!(DOC.contains(name), "docs/CATALOG.md should mention {name}");
        }
    }
}
