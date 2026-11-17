//! Filling dynamic product blocks and the `catalog` / `recommendations` template variables at
//! send time.
//!
//! [`personalize`] runs once per message before rendering. Content without a `products` block or
//! a reference to `catalog` or `recommendations` passes through untouched, without a query.
//! Product lists that are the same for everyone (hand-picked, a collection, best sellers, newest,
//! on sale) are cached in memory for a minute, so a big campaign doesn't query per recipient.

use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use once_cell::sync::Lazy;
use regex::Regex;
use serde_json::{json, Value};
use sqlx::PgPool;
use uuid::Uuid;

use super::recs;
use super::store::{self, card, Product, PRODUCT_COLS};
use crate::error::AppResult;
use crate::models::Profile;

pub const SOURCES: &[&str] = &["handpicked", "collection", "best_sellers", "newest", "on_sale", "event_items", "recently_viewed", "recommended"];
const MAX_COUNT: i64 = 12;
const SHARED_TTL: Duration = Duration::from_secs(60);
/// Products the Jinja `catalog` lists hold.
const LIST_SIZE: i64 = 8;

/// Check a `products` block's settings (when content is saved through the catalog tools and in tests).
pub fn check_block(b: &Value) -> Result<(), String> {
    let source = b["source"].as_str().unwrap_or("");
    if !SOURCES.contains(&source) {
        return Err(format!("products block source must be one of {}", SOURCES.join(", ")));
    }
    if let Some(f) = b["fallback"].as_str().filter(|f| !f.is_empty()) {
        if !SOURCES.contains(&f) {
            return Err(format!("products block fallback must be one of {}", SOURCES.join(", ")));
        }
    }
    if source == "handpicked" && b["product_ids"].as_array().is_none_or(|a| a.is_empty() || a.len() > MAX_COUNT as usize) {
        return Err("hand-picked products need 1 to 12 product ids".into());
    }
    if source == "collection" && ["category", "tag", "brand"].iter().all(|k| b[*k].as_str().is_none_or(str::is_empty)) {
        return Err("a collection needs a category, tag or brand".into());
    }
    if let Some(c) = b["count"].as_i64() {
        if !(1..=MAX_COUNT).contains(&c) {
            return Err("products block count must be 1 to 12".into());
        }
    }
    if let Some(c) = b["columns"].as_i64() {
        if !(1..=4).contains(&c) {
            return Err("products block columns must be 1 to 4".into());
        }
    }
    Ok(())
}

static SHARED: Lazy<Mutex<HashMap<String, (Instant, Vec<Value>)>>> = Lazy::new(|| Mutex::new(HashMap::new()));

fn shared_get(key: &str) -> Option<Vec<Value>> {
    let m = SHARED.lock().ok()?;
    m.get(key).filter(|(at, _)| at.elapsed() < SHARED_TTL).map(|(_, v)| v.clone())
}

fn shared_put(key: String, v: Vec<Value>) {
    if let Ok(mut m) = SHARED.lock() {
        if m.len() > 2000 {
            m.retain(|_, (at, _)| at.elapsed() < SHARED_TTL);
            if m.len() > 2000 {
                m.clear();
            }
        }
        m.insert(key, (Instant::now(), v));
    }
}

fn s<'a>(b: &'a Value, k: &str) -> Option<&'a str> {
    b.get(k).and_then(|v| v.as_str()).map(str::trim).filter(|s| !s.is_empty())
}

struct Spec<'a> {
    source: &'a str,
    block: &'a Value,
    count: i64,
    hide_oos: bool,
}

async fn list_query(db: &PgPool, account_id: Uuid, sql_where: &str, order: &str, binds: (&Option<String>, &Option<String>, &Option<String>), in_stock_only: bool, limit: i64) -> AppResult<Vec<Product>> {
    let q = format!(
        "SELECT {PRODUCT_COLS} FROM catalog_products cp WHERE cp.account_id = $1 AND cp.published AND (NOT $2 OR cp.in_stock)
           AND ($3::text IS NULL OR cp.categories @> ARRAY[$3]::text[]) AND ($4::text IS NULL OR cp.tags @> ARRAY[$4]::text[])
           AND ($5::text IS NULL OR lower(cp.brand) = lower($5)) {sql_where}
         ORDER BY {order} LIMIT $6"
    );
    Ok(sqlx::query_as(&q).bind(account_id).bind(in_stock_only).bind(binds.0).bind(binds.1).bind(binds.2).bind(limit).fetch_all(db).await?)
}

/// The products one source gives, as cards.
async fn source_cards(db: &PgPool, account_id: Uuid, profile: Option<&Profile>, event: Option<&Value>, spec: &Spec<'_>, preview: bool) -> AppResult<Vec<Value>> {
    let (count, oos) = (spec.count, spec.hide_oos);
    let shared_key = match spec.source {
        "handpicked" | "collection" | "best_sellers" | "newest" | "on_sale" => Some(format!(
            "{account_id}|{}|{}|{}|{}|{}|{}|{count}|{oos}",
            spec.source,
            spec.block.get("product_ids").map(|v| v.to_string()).unwrap_or_default(),
            s(spec.block, "category").unwrap_or(""),
            s(spec.block, "tag").unwrap_or(""),
            s(spec.block, "brand").unwrap_or(""),
            spec.block.get("days").and_then(|d| d.as_i64()).unwrap_or(30),
        )),
        _ => None,
    };
    if let Some(k) = &shared_key {
        if let Some(v) = shared_get(k) {
            return Ok(v);
        }
    }
    let none = (&None, &None, &None);
    let products: Vec<Product> = match spec.source {
        "handpicked" => {
            let ids: Vec<String> = spec.block["product_ids"].as_array().map(|a| a.iter().filter_map(|v| v.as_str().map(String::from).or_else(|| v.as_i64().map(|n| n.to_string()))).take(MAX_COUNT as usize).collect()).unwrap_or_default();
            let q = format!("SELECT {PRODUCT_COLS} FROM catalog_products cp WHERE cp.account_id = $1 AND cp.published AND (cp.external_id = ANY($2) OR cp.id::text = ANY($2))");
            let rows: Vec<Product> = sqlx::query_as(&q).bind(account_id).bind(&ids).fetch_all(db).await?;
            ids.iter().filter_map(|id| rows.iter().find(|p| &p.external_id == id || &p.id.to_string() == id).cloned()).filter(|p| !oos || p.in_stock).collect()
        }
        "collection" => {
            let binds = (s(spec.block, "category").map(String::from), s(spec.block, "tag").map(String::from), s(spec.block, "brand").map(String::from));
            list_query(db, account_id, "", "cp.published_at DESC, cp.id", (&binds.0, &binds.1, &binds.2), oos, count).await?
        }
        "best_sellers" => {
            let days = spec.block.get("days").and_then(|d| d.as_i64()).unwrap_or(30).clamp(1, 365) as i32;
            recs::best_sellers(db, account_id, days, count, oos, &[]).await?
        }
        "newest" => list_query(db, account_id, "", "cp.published_at DESC, cp.id", none, oos, count).await?,
        "on_sale" => list_query(db, account_id, "AND cp.compare_at_price > cp.price", "(cp.compare_at_price - cp.price) / NULLIF(cp.compare_at_price, 0) DESC, cp.id", none, oos, count).await?,
        "recently_viewed" => match profile {
            Some(p) => recs::recently_viewed(db, account_id, p.id, count, oos).await?,
            None => Vec::new(),
        },
        "recommended" => match profile {
            Some(p) => recs::for_person(db, account_id, p.id, count).await?,
            None => Vec::new(),
        },
        "event_items" => return event_item_cards(db, account_id, profile, event, count, oos, preview).await,
        _ => Vec::new(),
    };
    let cards: Vec<Value> = products.iter().take(count as usize).map(card).collect();
    if let Some(k) = shared_key {
        shared_put(k, cards.clone());
    }
    Ok(cards)
}

/// The triggering event's items. A preview has no event, so it uses this person's latest
/// cart, checkout or product view instead.
async fn event_item_cards(db: &PgPool, account_id: Uuid, profile: Option<&Profile>, event: Option<&Value>, count: i64, oos: bool, preview: bool) -> AppResult<Vec<Value>> {
    let sample;
    let event = match (event.filter(|e| e.get("properties").is_some()), profile) {
        (Some(e), _) => e,
        (None, Some(p)) if preview => {
            let row: Option<(String, Value)> = sqlx::query_as(
                "SELECT m.name, e.properties FROM events e JOIN metrics m ON m.id = e.metric_id
                 WHERE e.profile_id = $1 AND m.name IN ('Started Checkout', 'Added to Cart', 'Placed Order', 'Viewed Product')
                 ORDER BY e.occurred_at DESC LIMIT 1",
            )
            .bind(p.id)
            .fetch_optional(db)
            .await?;
            let Some((metric, props)) = row else { return Ok(Vec::new()) };
            sample = json!({"metric": metric, "properties": props});
            &sample
        }
        _ => return Ok(Vec::new()),
    };
    let items = super::learn::event_cards(db, account_id, event).await?;
    let ids: Vec<Uuid> = items.iter().filter_map(|(id, _)| *id).collect();
    let known = recs::products_by_ids(db, account_id, &ids, false).await?;
    let mut out = Vec::new();
    for (id, raw) in items {
        // A catalog match has the current price and image; keep the event's quantity.
        let c = match id.and_then(|id| known.iter().find(|p| p.id == id)) {
            Some(p) => {
                let mut c = card(p);
                c["quantity"] = raw["quantity"].clone();
                c
            }
            None if raw["title"].is_string() => raw,
            None => continue,
        };
        if oos && c["in_stock"] == json!(false) {
            continue;
        }
        if out.iter().any(|x: &Value| x["id"] == c["id"] && !c["id"].is_null()) {
            continue;
        }
        out.push(c);
        if out.len() as i64 >= count {
            break;
        }
    }
    Ok(out)
}

/// The products a block shows for one recipient, trying its fallback when the source is empty.
pub async fn resolve_block(db: &PgPool, account_id: Uuid, profile: Option<&Profile>, event: Option<&Value>, block: &Value, preview: bool) -> AppResult<Vec<Value>> {
    let count = block.get("count").and_then(|c| c.as_i64()).unwrap_or(4).clamp(1, MAX_COUNT);
    let hide_oos = block.get("hide_out_of_stock").and_then(|v| v.as_bool()).unwrap_or(true);
    let source = s(block, "source").unwrap_or("best_sellers");
    let spec = Spec { source, block, count, hide_oos };
    let mut items = source_cards(db, account_id, profile, event, &spec, preview).await?;
    if items.is_empty() {
        if let Some(fb) = s(block, "fallback").filter(|f| *f != source && SOURCES.contains(f)) {
            items = source_cards(db, account_id, profile, event, &Spec { source: fb, block, count, hide_oos }, preview).await?;
        }
    }
    Ok(items)
}

#[derive(Default)]
struct Refs {
    products_block: bool,
    catalog: bool,
    recommendations: bool,
}

fn scan(v: &Value, refs: &mut Refs, ids: &mut Vec<String>, depth: usize) {
    static ID: Lazy<Regex> = Lazy::new(|| Regex::new(r#"catalog\.products\[\s*["']([^"'\]]{1,255})["']\s*\]"#).expect("static regex"));
    // Only names used inside a template tag count, not the word in ordinary text.
    static CATALOG: Lazy<Regex> = Lazy::new(|| Regex::new(r"\{[{%][^}]*\bcatalog\b").expect("static regex"));
    static RECS: Lazy<Regex> = Lazy::new(|| Regex::new(r"\{[{%][^}]*\brecommendations\b").expect("static regex"));
    if depth > 8 {
        return;
    }
    match v {
        Value::String(t) => {
            if t.contains("catalog") && CATALOG.is_match(t) {
                refs.catalog = true;
                for c in ID.captures_iter(t).take(50) {
                    ids.push(c[1].to_string());
                }
            }
            if t.contains("recommendations") && RECS.is_match(t) {
                refs.recommendations = true;
            }
        }
        Value::Array(a) => a.iter().for_each(|x| scan(x, refs, ids, depth + 1)),
        Value::Object(o) => {
            if o.get("type").and_then(|t| t.as_str()) == Some("products") {
                refs.products_block = true;
            }
            o.iter().filter(|(k, _)| k.as_str() != "items").for_each(|(_, x)| scan(x, refs, ids, depth + 1));
        }
        _ => {}
    }
}

/// Fill a message's product blocks and catalog variables for one recipient. Returns the content
/// to render (unchanged and unqueried when it uses no catalog data).
pub async fn personalize<'a>(db: &PgPool, account_id: Uuid, profile: Option<&Profile>, event: Option<&Value>, content: &'a Value, ctx: &mut Value, preview: bool) -> AppResult<Cow<'a, Value>> {
    let mut refs = Refs::default();
    let mut ids = Vec::new();
    scan(content, &mut refs, &mut ids, 0);
    if !refs.products_block && !refs.catalog && !refs.recommendations {
        return Ok(Cow::Borrowed(content));
    }
    let mut out = content.clone();
    if refs.products_block {
        if let Some(blocks) = out.get_mut("blocks").and_then(|b| b.as_array_mut()) {
            for b in blocks.iter_mut().filter(|b| b["type"] == "products") {
                let items = resolve_block(db, account_id, profile, event, b, preview).await?;
                b["items"] = Value::Array(items);
            }
        }
    }
    if refs.catalog {
        let text = content.to_string();
        let mut catalog = serde_json::Map::new();
        if !ids.is_empty() {
            ids.sort();
            ids.dedup();
            let q = format!("SELECT {PRODUCT_COLS} FROM catalog_products cp WHERE cp.account_id = $1 AND cp.external_id = ANY($2) AND cp.published");
            let rows: Vec<Product> = sqlx::query_as(&q).bind(account_id).bind(&ids).fetch_all(db).await?;
            catalog.insert("products".into(), Value::Object(rows.iter().map(|p| (p.external_id.clone(), card(p))).collect()));
        }
        for (name, source) in [("best_sellers", "best_sellers"), ("newest", "newest"), ("on_sale", "on_sale")] {
            if text.contains(&format!("catalog.{name}")) {
                let block = json!({});
                let cards = source_cards(db, account_id, None, None, &Spec { source, block: &block, count: LIST_SIZE, hide_oos: true }, preview).await?;
                catalog.insert(name.into(), Value::Array(cards));
            }
        }
        ctx["catalog"] = Value::Object(catalog);
    }
    if refs.recommendations {
        let picks = match profile {
            Some(p) => recs::for_person(db, account_id, p.id, LIST_SIZE).await?,
            None => recs::best_sellers(db, account_id, 30, LIST_SIZE, true, &[]).await?,
        };
        ctx["recommendations"] = Value::Array(picks.iter().map(card).collect());
    }
    Ok(Cow::Owned(out))
}

/// Used by the dashboard's product picker and `get_product`.
pub fn cards(products: &[Product]) -> Vec<Value> {
    products.iter().map(store::card).collect()
}
