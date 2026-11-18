//! Dashboard and REST routes, and the MCP tools (the same actions).

use axum::extract::{DefaultBodyLimit, Path, State};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};
use uuid::Uuid;

use super::{customers, import, oauth, privacy, shop, webhooks};
use crate::api::ApiJson;
use crate::auth::Ctx;
use crate::error::{AppError, AppResult};
use crate::features::team::access::{public, rule, Perm, RouteRule, ToolRule};
use crate::features::team::audit;
use crate::mcp::{from, obj, tool};
use crate::state::AppState;

/// Who may do what (see `team::access`). Seeing the connection and its progress needs account
/// read; changing it, its import and product sync, and downloading a privacy export need
/// `shopify.manage` (admins and the owner). Shopify's own requests are public and signed; the
/// install link and callback check the signed-in member themselves (see `oauth`).
pub static ROUTES: &[RouteRule] = &[
    rule("GET", "/api/shopify", Perm::AccountRead),
    rule("POST", "/api/shopify/connect", Perm::ShopifyManage),
    rule("POST", "/api/shopify/disconnect", Perm::ShopifyManage),
    rule("GET", "/api/shopify/import", Perm::AccountRead),
    rule("POST", "/api/shopify/import", Perm::ShopifyManage),
    rule("POST", "/api/shopify/import/{action}", Perm::ShopifyManage),
    rule("POST", "/api/shopify/products/resync", Perm::ShopifyManage),
    rule("GET", "/api/shopify/data-requests/{id}", Perm::ShopifyManage),
    rule("PUT", "/api/shopify/coupons/{id}", Perm::ShopifyManage),
    rule("DELETE", "/api/shopify/coupons/{id}", Perm::ShopifyManage),
    public("GET", "/api/shopify/app"),
    public("GET", "/api/shopify/install"),
    public("GET", "/api/shopify/callback"),
    public("POST", "/webhooks/shopify"),
];

pub static TOOLS: &[ToolRule] = &[
    ToolRule { name: "get_shopify_status", perm: Perm::AccountRead },
    ToolRule { name: "get_shopify_import", perm: Perm::AccountRead },
    ToolRule { name: "connect_shopify", perm: Perm::ShopifyManage },
    ToolRule { name: "disconnect_shopify", perm: Perm::ShopifyManage },
    ToolRule { name: "start_shopify_import", perm: Perm::ShopifyManage },
    ToolRule { name: "pause_shopify_import", perm: Perm::ShopifyManage },
    ToolRule { name: "resume_shopify_import", perm: Perm::ShopifyManage },
    ToolRule { name: "cancel_shopify_import", perm: Perm::ShopifyManage },
    ToolRule { name: "resync_shopify_products", perm: Perm::ShopifyManage },
    ToolRule { name: "get_shopify_data_request", perm: Perm::ShopifyManage },
    ToolRule { name: "set_shopify_coupon", perm: Perm::ShopifyManage },
    ToolRule { name: "remove_shopify_coupon", perm: Perm::ShopifyManage },
];

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/shopify", get(status_h))
        .route("/api/shopify/connect", post(connect_h))
        .route("/api/shopify/disconnect", post(disconnect_h))
        .route("/api/shopify/import", get(get_import_h).post(start_import_h))
        .route("/api/shopify/import/{action}", post(import_action_h))
        .route("/api/shopify/products/resync", post(resync_h))
        .route("/api/shopify/data-requests/{id}", get(data_request_h))
        .route("/api/shopify/coupons/{id}", axum::routing::put(set_coupon_h).delete(remove_coupon_h))
        // Public: signed by Shopify.
        .route("/api/shopify/app", get(oauth::app_entry))
        .route("/api/shopify/install", get(oauth::install))
        .route("/api/shopify/callback", get(oauth::callback))
        .route("/webhooks/shopify", post(webhooks::receive).layer(DefaultBodyLimit::max(webhooks::MAX_BODY)))
}

/// Settings → Shopify: the connection, what's syncing, import progress, last webhook and the health line.
pub async fn status(state: &AppState, account_id: Uuid) -> AppResult<Value> {
    let s = shop::load(&state.db, account_id).await?;
    let connected = s.as_ref().is_some_and(|s| s.status == "connected");
    let history = import::latest(&state.db, account_id, "history").await?;
    let products = import::latest(&state.db, account_id, "products").await?;
    let (linked, catalog, waiting): (i64, i64, i64) = sqlx::query_as(
        "SELECT (SELECT count(*) FROM shopify_customers WHERE account_id = $1 AND profile_id IS NOT NULL),
                (SELECT count(*) FROM catalog_products WHERE account_id = $1 AND source = 'shopify'),
                (SELECT count(*) FROM shopify_webhooks WHERE account_id = $1 AND done_at IS NULL)",
    )
    .bind(account_id)
    .fetch_one(&state.db)
    .await?;
    let browser_orders: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM events e JOIN metrics m ON m.id = e.metric_id
         WHERE e.account_id = $1 AND m.account_id = $1 AND m.name = 'Placed Order' AND e.from_browser AND e.occurred_at > now() - interval '30 days')",
    )
    .bind(account_id)
    .fetch_one(&state.db)
    .await?;
    let last_order = s.as_ref().and_then(|s| s.last_order_at);
    let missing: Vec<String> = s.as_ref().filter(|_| connected).map(|s| s.missing_topics.clone()).unwrap_or_default();
    let not_receiving = format!("Connected, but not receiving {} from Shopify. DumbMail keeps trying to subscribe.", missing.join(", "));
    let (health, message) = match (connected, last_order) {
        (true, _) if !missing.is_empty() => ("incomplete", not_receiving.as_str()),
        (true, Some(t)) if t > chrono::Utc::now() - chrono::Duration::days(7) => ("server", "Orders are arriving from Shopify."),
        (true, Some(_)) => ("quiet", "Connected. No orders have come from Shopify in the last 7 days."),
        (true, None) => ("waiting", "Connected. Orders will show here as they come in from Shopify."),
        (false, _) if browser_orders => ("browser_only", "Only the browser snippet is reporting orders. Connect your store so every order, refund and shipment comes from Shopify itself."),
        (false, _) => ("none", "No orders are being reported yet."),
    };
    let embed_link = match (&s, state.cfg.shopify.key()) {
        (Some(s), Some(key)) if connected => Some(format!("https://{}/admin/themes/current/editor?context=apps&activateAppId={key}/dumbmail", s.shop_domain)),
        _ => None,
    };
    let data_requests = privacy::list(&state.db, account_id).await?;
    let (privacy_waiting, privacy_overdue) = privacy::pending(&state.db, account_id).await?;
    let (writeback_waiting, writeback_failing) = match &s {
        Some(s) if connected => customers::writeback_counts(&state.db, account_id, &s.shop_domain).await?,
        _ => (0, 0),
    };
    let mut problems: Vec<String> = Vec::new();
    if privacy_overdue > 0 {
        problems.push(format!("{privacy_overdue} Shopify privacy request(s) have been failing for over {} days. Shopify expects them done within 30 days.", privacy::ALERT_DAYS));
    }
    if writeback_failing > 0 {
        problems.push(format!("{writeback_failing} unsubscribe(s) haven't been accepted by Shopify yet. DumbMail keeps trying."));
    }
    if let Some(c) = s.as_ref().and_then(|s| s.cleanup_error.clone()) {
        problems.push(c);
    }
    if let Some(n) = s.as_ref().map(|s| s.dropped_payloads).filter(|n| *n > 0) {
        problems.push(format!("{n} webhook(s) from Shopify were deleted unprocessed after 30 days."));
    }
    Ok(json!({
        "available": state.cfg.shopify.available(),
        "connection": s.as_ref().map(shop::status_json),
        "connected": connected,
        "health": {"state": health, "message": message},
        "syncing": if connected { json!(["Orders, refunds, shipments and cancellations", "Checkouts (for abandoned cart flows)", "Customers and their email and SMS consent", "Products, prices and stock", "Unsubscribes, written back to Shopify"]) } else { json!([]) },
        "counts": {"linked_customers": linked, "products": catalog, "webhooks_waiting": waiting, "privacy_requests_waiting": privacy_waiting, "unsubscribes_to_write_back": writeback_waiting},
        "problems": problems,
        "import": history.as_ref().map(import::to_json),
        "products_sync": products.as_ref().map(import::to_json),
        "storefront": {"embed_link": embed_link, "pixel_installed": s.as_ref().is_some_and(|s| s.web_pixel_id.is_some()), "error": s.as_ref().and_then(|s| s.storefront_error.clone())},
        "data_requests": data_requests,
        "coupons": super::coupons::list(&state.db, account_id).await?,
        "webhook_url": oauth::webhook_uri(state),
        "redirect_url": oauth::redirect_uri(state),
        "app_url": format!("{}/api/shopify/app", state.cfg.public_url()),
    }))
}

async fn status_h(State(state): State<AppState>, ctx: Ctx) -> AppResult<Json<Value>> {
    Ok(Json(status(&state, ctx.account_id).await?))
}

#[derive(Deserialize)]
struct ConnectBody {
    shop: String,
}

async fn connect_h(State(state): State<AppState>, ctx: Ctx, ApiJson(b): ApiJson<ConnectBody>) -> AppResult<Json<Value>> {
    Ok(Json(oauth::start(&state, &ctx, &b.shop).await?))
}

#[derive(Deserialize, Default)]
struct ConfirmBody {
    #[serde(default)]
    confirm: bool,
    #[serde(default)]
    order_days: Option<i64>,
}

async fn disconnect(state: &AppState, ctx: &Ctx, confirm: bool) -> AppResult<Value> {
    if !confirm {
        let s = shop::connected(&state.db, ctx.account_id).await?;
        return Ok(json!({"dry_run": true, "note": format!("Nothing changed. Disconnecting {} removes DumbMail's webhooks, pixel and theme embed settings from the store and forgets its access. Profiles, events and products already brought in stay. Call again with confirm=true once the user approves.", s.shop_domain)}));
    }
    let r = oauth::disconnect(state, ctx.account_id).await?;
    audit(state, ctx, "shopify.disconnect", None, json!({"cleaned_up": r["cleaned_up"]})).await;
    Ok(r)
}

async fn disconnect_h(State(state): State<AppState>, ctx: Ctx, ApiJson(b): ApiJson<ConfirmBody>) -> AppResult<Json<Value>> {
    Ok(Json(disconnect(&state, &ctx, b.confirm).await?))
}

async fn start_import(state: &AppState, ctx: &Ctx, confirm: bool, order_days: Option<i64>) -> AppResult<Value> {
    let s = shop::connected(&state.db, ctx.account_id).await?;
    if !confirm {
        let days = order_days.unwrap_or(import::DEFAULT_ORDER_YEARS * 365);
        return Ok(json!({"dry_run": true, "note": format!(
            "Nothing started. This reads every customer and product from {} and its orders from the last {days} days, in the background. Customers subscribed in Shopify are subscribed here; nobody else is. Imported orders don't start flows, send anything or cost anything. Running it again adds nothing twice. Call again with confirm=true once the user approves.",
            s.shop_domain
        )}));
    }
    let r = import::start(state, ctx.account_id, ctx.user_id, "history", order_days).await?;
    audit(state, ctx, "shopify.import_start", Some(&r.id.to_string()), json!({"orders_since": r.orders_since})).await;
    Ok(import::to_json(&r))
}

/// Pause, resume or cancel the import (recorded in the audit log).
async fn import_action(state: &AppState, ctx: &Ctx, action: &str) -> AppResult<Value> {
    let r = import::set_status(state, ctx.account_id, action).await?;
    audit(state, ctx, &format!("shopify.import_{action}"), Some(&r.id.to_string()), json!({"kind": r.kind})).await;
    Ok(import::to_json(&r))
}

/// A privacy export (personal data): recorded in the audit log, without its contents.
async fn data_request(state: &AppState, ctx: &Ctx, id: Uuid) -> AppResult<Value> {
    let export = privacy::get(&state.db, ctx.account_id, id).await?.ok_or_else(|| AppError::not_found("data request"))?;
    audit(state, ctx, "shopify.privacy_export", Some(&id.to_string()), json!({})).await;
    Ok(export)
}

async fn start_import_h(State(state): State<AppState>, ctx: Ctx, ApiJson(b): ApiJson<ConfirmBody>) -> AppResult<Json<Value>> {
    Ok(Json(start_import(&state, &ctx, b.confirm, b.order_days).await?))
}

async fn get_import(state: &AppState, account_id: Uuid) -> AppResult<Value> {
    Ok(json!({
        "import": import::latest(&state.db, account_id, "history").await?.as_ref().map(import::to_json),
        "products_sync": import::latest(&state.db, account_id, "products").await?.as_ref().map(import::to_json),
    }))
}

async fn get_import_h(State(state): State<AppState>, ctx: Ctx) -> AppResult<Json<Value>> {
    Ok(Json(get_import(&state, ctx.account_id).await?))
}

async fn import_action_h(State(state): State<AppState>, ctx: Ctx, Path(action): Path<String>) -> AppResult<Json<Value>> {
    Ok(Json(import_action(&state, &ctx, &action).await?))
}

async fn resync(state: &AppState, ctx: &Ctx) -> AppResult<Value> {
    let r = import::start(state, ctx.account_id, ctx.user_id, "products", None).await?;
    Ok(json!({"products_sync": import::to_json(&r), "note": "Reading every product from Shopify again in the background."}))
}

async fn resync_h(State(state): State<AppState>, ctx: Ctx) -> AppResult<Json<Value>> {
    Ok(Json(resync(&state, &ctx).await?))
}

async fn data_request_h(State(state): State<AppState>, ctx: Ctx, Path(id): Path<Uuid>) -> AppResult<Json<Value>> {
    Ok(Json(data_request(&state, &ctx, id).await?))
}

async fn set_coupon(state: &AppState, ctx: &Ctx, id: Uuid, body: &Value) -> AppResult<Value> {
    let r = super::coupons::set(state, ctx.account_id, id, body).await?;
    audit(state, ctx, "shopify.coupon_link", Some(&id.to_string()), json!({})).await;
    Ok(r)
}

async fn remove_coupon(state: &AppState, ctx: &Ctx, id: Uuid) -> AppResult<Value> {
    let r = super::coupons::remove(state, ctx.account_id, id).await?;
    audit(state, ctx, "shopify.coupon_unlink", Some(&id.to_string()), json!({})).await;
    Ok(r)
}

async fn set_coupon_h(State(state): State<AppState>, ctx: Ctx, Path(id): Path<Uuid>, ApiJson(b): ApiJson<Value>) -> AppResult<Json<Value>> {
    Ok(Json(set_coupon(&state, &ctx, id, &b).await?))
}

async fn remove_coupon_h(State(state): State<AppState>, ctx: Ctx, Path(id): Path<Uuid>) -> AppResult<Json<Value>> {
    Ok(Json(remove_coupon(&state, &ctx, id).await?))
}

pub fn tools() -> Vec<Value> {
    let confirm = json!({"type": "boolean", "description": "true to do it. Without it, only describes what would happen."});
    vec![
        tool("get_shopify_status", "Get Shopify status", "The connected Shopify store (if any), what syncs, the last webhook, import and product sync progress, problems in plain words, data requests waiting, and whether orders are arriving from Shopify or only from the browser snippet.", obj(json!({}), &[]), true, false),
        tool("connect_shopify", "Connect a Shopify store", "Start connecting a Shopify store (one per account). Returns a DumbMail link to open in a browser signed in as a member who may manage the Shopify connection (an admin or the owner); it goes on to Shopify, where the merchant approves the app, and comes back. Only that browser can finish it.",
            obj(json!({"shop": {"type": "string", "description": "The store's myshopify.com address (your-store.myshopify.com) or name.", "maxLength": 200}}), &["shop"]), false, false),
        tool("disconnect_shopify", "Disconnect the Shopify store", "Remove DumbMail's webhooks, pixel and theme embed settings from the store and forget its access. Says what Shopify didn't confirm removing. Profiles, events and products stay. Without confirm=true it only describes what would happen.",
            obj(json!({"confirm": confirm}), &[]), false, true),
        tool("start_shopify_import", "Import Shopify history", "Read every customer and product, and orders from the last order_days days (default 3 years), in the background. Consent comes only from Shopify's own consent fields. Imported orders don't start flows, send or bill anything. Without confirm=true it only describes what would happen.",
            obj(json!({"confirm": confirm, "order_days": {"type": "integer", "minimum": 1, "maximum": 7300}}), &[]), false, true),
        tool("get_shopify_import", "Get Shopify import progress", "The latest history import and product sync: phase, counts, status and any error.", obj(json!({}), &[]), true, false),
        tool("pause_shopify_import", "Pause the Shopify import", "Pause the running history import; resume picks up where it stopped.", obj(json!({}), &[]), false, false),
        tool("resume_shopify_import", "Resume the Shopify import", "Resume a paused history import.", obj(json!({}), &[]), false, false),
        tool("cancel_shopify_import", "Cancel the Shopify import", "Stop the running or paused import. What it already brought in stays; starting again adds nothing twice.", obj(json!({}), &[]), false, false),
        tool("resync_shopify_products", "Sync Shopify products now", "Read every product, price and stock level from Shopify again now (it also happens daily, and webhooks keep products current in between).", obj(json!({}), &[]), false, false),
        tool("get_shopify_data_request", "Export a Shopify data request", "Return the personal data DumbMail prepared for a Shopify customer's data request (customers/data_request), to hand to that customer, and record that it was downloaded. Listed in get_shopify_status.",
            obj(json!({"id": {"type": "string"}}), &["id"]), false, false),
        tool("set_shopify_coupon", "Make a coupon's codes in Shopify", "Link a coupon (Content → Coupons) to the connected store: when the coupon has no free code for someone, each person gets their own single-use Shopify discount code made from this template, keyed by store, coupon, person and send.",
            obj(json!({
                "coupon_id": {"type": "string"},
                "title": {"type": "string", "maxLength": 255, "description": "Shown in Shopify and at checkout."},
                "value": {"type": "object", "description": "{\"percentage\": 10} (0-100) or {\"fixed_amount\": 5} in the store's currency."},
                "minimum_subtotal": {"type": "number", "minimum": 0},
                "collection_ids": {"type": "array", "items": {"type": "string"}, "maxItems": 100, "description": "Shopify collection ids; empty means everything."},
                "prefix": {"type": "string", "maxLength": 12, "description": "Letters and digits at the start of each code."},
                "combines_with_other_discounts": {"type": "boolean"},
                "expires_after_days": {"type": "integer", "minimum": 1, "maximum": 3650, "description": "Otherwise the coupon's own expiry."}
            }), &["coupon_id", "title", "value"]), false, false),
        tool("remove_shopify_coupon", "Stop making a coupon's codes in Shopify", "Unlink a coupon from the store. Codes already given stay theirs.", obj(json!({"coupon_id": {"type": "string"}}), &["coupon_id"]), false, false),
    ]
}

pub async fn call(state: &AppState, ctx: &Ctx, name: &str, a: &Value) -> AppResult<Value> {
    let acct = ctx.account_id;
    let confirm = a["confirm"].as_bool() == Some(true);
    match name {
        "get_shopify_status" => status(state, acct).await,
        "connect_shopify" => {
            let b: ConnectBody = from(json!({"shop": a["shop"]}))?;
            oauth::start(state, ctx, &b.shop).await
        }
        "disconnect_shopify" => disconnect(state, ctx, confirm).await,
        "start_shopify_import" => start_import(state, ctx, confirm, a["order_days"].as_i64()).await,
        "get_shopify_import" => get_import(state, acct).await,
        "pause_shopify_import" => import_action(state, ctx, "pause").await,
        "resume_shopify_import" => import_action(state, ctx, "resume").await,
        "cancel_shopify_import" => import_action(state, ctx, "cancel").await,
        "resync_shopify_products" => resync(state, ctx).await,
        "get_shopify_data_request" => {
            let id: Uuid = a["id"].as_str().and_then(|s| s.parse().ok()).ok_or_else(|| AppError::bad("id must be an id"))?;
            data_request(state, ctx, id).await
        }
        "set_shopify_coupon" | "remove_shopify_coupon" => {
            let id: Uuid = a["coupon_id"].as_str().and_then(|s| s.parse().ok()).ok_or_else(|| AppError::bad("coupon_id must be an id"))?;
            if name == "remove_shopify_coupon" {
                return remove_coupon(state, ctx, id).await;
            }
            let mut body = a.clone();
            if let Some(o) = body.as_object_mut() {
                o.remove("coupon_id");
            }
            set_coupon(state, ctx, id, &body).await
        }
        _ => Err(AppError::not_found("tool")),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn tools_have_object_schemas_and_docs_name_them() {
        let doc = include_str!("../../../docs/SHOPIFY.md");
        for t in super::tools() {
            assert_eq!(t["inputSchema"]["type"], "object", "{}", t["name"]);
            let name = t["name"].as_str().unwrap();
            assert!(doc.contains(name), "docs/SHOPIFY.md doesn't mention {name}");
        }
    }
}
