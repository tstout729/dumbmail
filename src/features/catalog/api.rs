//! REST routes for the catalog. The MCP tools call the same functions.

use axum::extract::{DefaultBodyLimit, Path, Query, State};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};
use uuid::Uuid;

use super::{alerts, blocks, feeds, recs, store};
use crate::api::ApiJson;
use crate::auth::Ctx;
use crate::error::{AppError, AppResult};
use crate::state::AppState;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/catalog/summary", get(summary_h))
        .route("/api/catalog/facets", get(facets_h))
        .route("/api/catalog/products", get(list_h).post(upsert_h).layer(DefaultBodyLimit::max(25 * 1024 * 1024)))
        .route("/api/catalog/products/import", post(import_h).layer(DefaultBodyLimit::max(25 * 1024 * 1024)))
        .route("/api/catalog/products/{key}", get(get_h).delete(delete_h))
        .route("/api/catalog/recommendations", get(recs_h))
        .route("/api/catalog/feeds", get(feeds_h).post(create_feed_h))
        .route("/api/catalog/feeds/{id}", get(get_feed_h).patch(update_feed_h).delete(delete_feed_h))
        .route("/api/catalog/feeds/{id}/sync", post(sync_feed_h))
        .route("/api/catalog/settings", get(settings_h).patch(update_settings_h))
        .route("/client/back-in-stock.js", get(alerts::widget_js))
        .route("/client/products/stock", get(alerts::stock_status).options(alerts::preflight))
        .route("/client/back-in-stock", post(alerts::subscribe_public).options(alerts::preflight).layer(DefaultBodyLimit::max(4 * 1024)))
        .route("/client/back-in-stock/confirm/{token}", get(alerts::confirm_page).post(alerts::confirm_post))
}

async fn summary_h(State(state): State<AppState>, ctx: Ctx) -> AppResult<Json<Value>> {
    Ok(Json(store::summary(&state.db, ctx.account_id).await?))
}

async fn facets_h(State(state): State<AppState>, ctx: Ctx) -> AppResult<Json<Value>> {
    Ok(Json(store::facets(&state.db, ctx.account_id).await?))
}

pub async fn list_products(state: &AppState, account_id: Uuid, q: &store::ListQuery) -> AppResult<Value> {
    let (rows, total) = store::list(&state.db, account_id, q).await?;
    Ok(json!({"data": rows, "total": total}))
}

async fn list_h(State(state): State<AppState>, ctx: Ctx, Query(q): Query<store::ListQuery>) -> AppResult<Json<Value>> {
    Ok(Json(list_products(&state, ctx.account_id, &q).await?))
}

/// One product object, a list, or `{"products": [...]}`.
pub fn products_from(body: Value) -> AppResult<Vec<store::ProductInput>> {
    let list = match body {
        Value::Array(a) => a,
        Value::Object(mut o) if o.contains_key("products") => match o.remove("products") {
            Some(Value::Array(a)) => a,
            _ => return Err(AppError::bad("products must be a list")),
        },
        obj @ Value::Object(_) => vec![obj],
        _ => return Err(AppError::bad("send a product, a list of products, or {\"products\": [...]}")),
    };
    if list.is_empty() {
        return Err(AppError::bad("no products to save"));
    }
    if list.len() > store::MAX_BATCH {
        return Err(AppError::bad(format!("send at most {} products per call", store::MAX_BATCH)));
    }
    list.into_iter()
        .enumerate()
        .map(|(i, v)| serde_json::from_value(v).map_err(|e| AppError::bad(format!("product {}: {e}", i + 1))))
        .collect()
}

async fn upsert_h(State(state): State<AppState>, ctx: Ctx, ApiJson(b): ApiJson<Value>) -> AppResult<Json<Value>> {
    let items = products_from(b)?;
    let s = store::upsert_products(&state.db, ctx.account_id, &items, store::Origin::Api).await?;
    Ok(Json(json!(s)))
}

#[derive(Deserialize)]
pub struct ImportBody {
    pub csv: String,
}

pub async fn import_csv(state: &AppState, account_id: Uuid, csv: &str) -> AppResult<Value> {
    if csv.len() > 20 * 1024 * 1024 {
        return Err(AppError::bad("the CSV is larger than 20 MB; split it"));
    }
    let (items, parse_errors) = feeds::parse_csv(csv).map_err(AppError::BadRequest)?;
    if items.is_empty() {
        return Err(AppError::bad(parse_errors.first().cloned().unwrap_or_else(|| "the CSV has no products".into())));
    }
    let mut s = store::upsert_products(&state.db, account_id, &items, store::Origin::Csv).await?;
    for e in parse_errors {
        s.error(e);
    }
    s.ids.clear();
    Ok(json!(s))
}

async fn import_h(State(state): State<AppState>, ctx: Ctx, ApiJson(b): ApiJson<ImportBody>) -> AppResult<Json<Value>> {
    Ok(Json(import_csv(&state, ctx.account_id, &b.csv).await?))
}

/// A product's detail. Who's waiting for a restock (people's emails) only for callers who may
/// read profiles; others get the counts.
pub async fn product_detail(state: &AppState, ctx: &Ctx, key: &str) -> AppResult<Value> {
    let mut v = store::detail(&state.db, ctx.account_id, key).await?;
    if !ctx.can(crate::features::team::Perm::ProfilesRead) {
        if let Some(o) = v.as_object_mut() {
            o.remove("waiting");
        }
    }
    Ok(v)
}

async fn get_h(State(state): State<AppState>, ctx: Ctx, Path(key): Path<String>) -> AppResult<Json<Value>> {
    Ok(Json(product_detail(&state, &ctx, &key).await?))
}

async fn delete_h(State(state): State<AppState>, ctx: Ctx, Path(key): Path<String>) -> AppResult<Json<Value>> {
    let p = store::delete(&state.db, ctx.account_id, &key).await?;
    crate::features::team::audit(&state, &ctx, "product.delete", Some(&p.id.to_string()), json!({"title": p.title})).await;
    Ok(Json(json!({"ok": true, "deleted": p.id})))
}

#[derive(Deserialize)]
pub struct RecsQuery {
    #[serde(default)]
    pub profile_id: Option<Uuid>,
    #[serde(default)]
    pub product_id: Option<String>,
    #[serde(default)]
    pub limit: Option<i64>,
}

/// Picks for a person (what they viewed and bought shows through, so it needs profiles.read), or
/// products bought and viewed together with a product.
pub async fn recommendations_for(state: &AppState, ctx: &Ctx, q: &RecsQuery) -> AppResult<Value> {
    if q.profile_id.is_some() {
        ctx.require(crate::features::team::Perm::ProfilesRead)?;
    }
    recommendations(state, ctx.account_id, q).await
}

/// [`recommendations_for`] without the permission check, for code acting for the account itself.
pub async fn recommendations(state: &AppState, account_id: Uuid, q: &RecsQuery) -> AppResult<Value> {
    let limit = q.limit.unwrap_or(8).clamp(1, 24);
    match (q.profile_id, q.product_id.as_deref()) {
        (Some(pid), None) => {
            crate::api::audience::load_profile(state, account_id, pid).await?;
            let picks = recs::for_person(&state.db, account_id, pid, limit).await?;
            let viewed: Vec<store::Product> = recs::recently_viewed(&state.db, account_id, pid, limit, false).await?.into_iter().map(|r| r.0).collect();
            Ok(json!({"for_person": blocks::cards(&picks), "recently_viewed": blocks::cards(&viewed),
                      "how": "Products bought or viewed together with this person's, then popular products in the categories they browse, leaving out what they bought, then best sellers."}))
        }
        (None, Some(key)) => {
            let p = store::find(&state.db, account_id, key).await?;
            let (bought, viewed) = recs::for_product(&state.db, account_id, p.id, limit).await?;
            Ok(json!({"product": store::card(&p), "bought_together": blocks::cards(&bought), "viewed_together": blocks::cards(&viewed),
                      "how": "Bought together counts orders that had both products; viewed together counts people who viewed both in the last 90 days."}))
        }
        _ => Err(AppError::bad("give a profile_id or a product_id (not both)")),
    }
}

async fn recs_h(State(state): State<AppState>, ctx: Ctx, Query(q): Query<RecsQuery>) -> AppResult<Json<Value>> {
    Ok(Json(recommendations_for(&state, &ctx, &q).await?))
}

async fn feeds_h(State(state): State<AppState>, ctx: Ctx) -> AppResult<Json<Value>> {
    Ok(Json(json!({"data": feeds::list(&state.db, ctx.account_id).await?})))
}

async fn create_feed_h(State(state): State<AppState>, ctx: Ctx, ApiJson(b): ApiJson<feeds::FeedInput>) -> AppResult<Json<Value>> {
    let f = feeds::create(&state.db, ctx.account_id, b).await?;
    feed_changed(&state, &ctx, "product_feed.create", &f).await;
    Ok(Json(json!(f)))
}

/// A feed was added or changed: its id, name, format and schedule (never its URL, which may hold
/// a token).
pub async fn feed_changed(state: &AppState, ctx: &Ctx, action: &str, f: &feeds::Feed) {
    crate::features::team::audit(state, ctx, action, Some(&f.id.to_string()), json!({"name": f.name, "format": f.format, "interval_hours": f.interval_hours, "enabled": f.enabled})).await;
}

async fn get_feed_h(State(state): State<AppState>, ctx: Ctx, Path(id): Path<Uuid>) -> AppResult<Json<Value>> {
    Ok(Json(json!(feeds::get(&state.db, ctx.account_id, id).await?)))
}

async fn update_feed_h(State(state): State<AppState>, ctx: Ctx, Path(id): Path<Uuid>, ApiJson(b): ApiJson<feeds::FeedInput>) -> AppResult<Json<Value>> {
    let f = feeds::update(&state.db, ctx.account_id, id, b).await?;
    feed_changed(&state, &ctx, "product_feed.update", &f).await;
    Ok(Json(json!(f)))
}

async fn delete_feed_h(State(state): State<AppState>, ctx: Ctx, Path(id): Path<Uuid>) -> AppResult<Json<Value>> {
    feeds::delete(&state.db, ctx.account_id, id).await?;
    crate::features::team::audit(&state, &ctx, "product_feed.delete", Some(&id.to_string()), json!({})).await;
    Ok(Json(json!({"ok": true})))
}

pub async fn sync_feed(state: &AppState, account_id: Uuid, id: Uuid) -> AppResult<Value> {
    feeds::get(&state.db, account_id, id).await?;
    let queued = feeds::queue_sync(&state.db, id, true).await?;
    Ok(json!({"ok": true, "queued": queued, "note": if queued { "Sync started. It runs in the background; check the feed's last_status in a minute." } else { "A sync is already waiting to run." }}))
}

async fn sync_feed_h(State(state): State<AppState>, ctx: Ctx, Path(id): Path<Uuid>) -> AppResult<Json<Value>> {
    Ok(Json(sync_feed(&state, ctx.account_id, id).await?))
}

/// Settings plus the widget's install snippet.
pub async fn settings_json(state: &AppState, account_id: Uuid) -> AppResult<Value> {
    let s = alerts::settings(&state.db, account_id).await?;
    let key: String = sqlx::query_scalar("SELECT public_key FROM accounts WHERE id = $1").bind(account_id).fetch_one(&state.db).await?;
    let mut v = json!(s);
    v["widget_snippet"] = json!(format!(r#"<script src="{}/client/back-in-stock.js?k={key}" async></script>"#, state.cfg.public_url()));
    Ok(v)
}

async fn settings_h(State(state): State<AppState>, ctx: Ctx) -> AppResult<Json<Value>> {
    Ok(Json(settings_json(&state, ctx.account_id).await?))
}

async fn update_settings_h(State(state): State<AppState>, ctx: Ctx, ApiJson(b): ApiJson<alerts::SettingsPatch>) -> AppResult<Json<Value>> {
    alerts::update_settings(&state.db, ctx.account_id, b).await?;
    crate::features::team::audit(&state, &ctx, "catalog.settings_change", None, json!({})).await;
    Ok(Json(settings_json(&state, ctx.account_id).await?))
}
