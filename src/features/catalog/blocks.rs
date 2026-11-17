//! Filling dynamic product blocks and the catalog template variables at send time.
//!
//! [`personalize`] runs once per message before rendering. Content without a `products` block or
//! a reference to the catalog variables passes through untouched, without a query. Product lists
//! that are the same for everyone (hand-picked, a collection, best sellers, newest, on sale, and
//! `catalog.products[...]` lookups) are cached in memory for a minute, so a big campaign doesn't
//! query per recipient.
//!
//! Template variables: `organization.catalog` (`.products["ID"]`, `.best_sellers`, `.newest`,
//! `.on_sale`) and `person.recommendations`. Profile properties are top-level template variables,
//! so the short names `catalog` and `recommendations` are filled too, but only on accounts where no
//! profile has a property by that name: an existing template using such a property is unchanged.
//!
//! Products learned from browser events and not yet verified only ever show to the person whose
//! own event carried them, using what that event said: in "items in the triggering event" and in
//! their own "recently viewed". Every other list holds verified products only.

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
use crate::models::{Account, Profile};

pub const SOURCES: &[&str] = &["handpicked", "collection", "best_sellers", "newest", "on_sale", "event_items", "recently_viewed", "recommended"];
const MAX_COUNT: i64 = 12;
const SHARED_TTL: Duration = Duration::from_secs(60);
const NAME_CHECK_TTL: Duration = Duration::from_secs(600);
/// Products the Jinja `catalog` lists hold.
const LIST_SIZE: i64 = 8;

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
        "SELECT {PRODUCT_COLS} FROM catalog_products cp WHERE cp.account_id = $1 AND cp.published AND cp.verified AND (NOT $2 OR cp.in_stock)
           AND ($3::text IS NULL OR cp.categories @> ARRAY[$3]::text[]) AND ($4::text IS NULL OR cp.tags @> ARRAY[$4]::text[])
           AND ($5::text IS NULL OR lower(cp.brand) = lower($5)) {sql_where}
         ORDER BY {order} LIMIT $6"
    );
    Ok(sqlx::query_as(&q).bind(account_id).bind(in_stock_only).bind(binds.0).bind(binds.1).bind(binds.2).bind(limit).fetch_all(db).await?)
}

/// Verified, published products by the store's id (or DumbMail id), cached like the shared lists.
async fn products_by_keys(db: &PgPool, account_id: Uuid, keys: &[String]) -> AppResult<Vec<Value>> {
    let cache_key = format!("{account_id}|keys|{}", serde_json::to_string(keys).unwrap_or_default());
    if let Some(v) = shared_get(&cache_key) {
        return Ok(v);
    }
    let q = format!("SELECT {PRODUCT_COLS} FROM catalog_products cp WHERE cp.account_id = $1 AND cp.published AND cp.verified AND (cp.external_id = ANY($2) OR cp.id::text = ANY($2))");
    let rows: Vec<Product> = sqlx::query_as(&q).bind(account_id).bind(keys).fetch_all(db).await?;
    let cards: Vec<Value> = keys
        .iter()
        .filter_map(|k| rows.iter().find(|p| &p.external_id == k || &p.id.to_string() == k))
        .map(|p| {
            let mut c = card(p);
            c["_key"] = json!(p.external_id);
            c["_uuid"] = json!(p.id.to_string());
            c
        })
        .collect();
    shared_put(cache_key, cards.clone());
    Ok(cards)
}

fn strip_keys(mut c: Value) -> Value {
    if let Some(o) = c.as_object_mut() {
        o.remove("_key");
        o.remove("_uuid");
    }
    c
}

/// The products one source gives, as cards.
async fn source_cards(db: &PgPool, account: &Account, profile: Option<&Profile>, event: Option<&Value>, spec: &Spec<'_>, preview: bool) -> AppResult<Vec<Value>> {
    let account_id = account.id;
    let (count, oos) = (spec.count, spec.hide_oos);
    let shared_key = match spec.source {
        "collection" | "best_sellers" | "newest" | "on_sale" => Some(format!(
            "{account_id}|{}|{}|{}|{}|{}|{count}|{oos}",
            spec.source,
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
            let cards = products_by_keys(db, account_id, &ids).await?;
            return Ok(cards.into_iter().filter(|c| !oos || c["in_stock"] != json!(false)).take(count as usize).map(strip_keys).collect());
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
        "recently_viewed" => {
            return match profile {
                Some(p) => recently_viewed_cards(db, account, p.id, count, oos).await,
                None => Ok(Vec::new()),
            }
        }
        "recommended" => match profile {
            Some(p) => recs::for_person(db, account_id, p.id, count).await?,
            None => Vec::new(),
        },
        "event_items" => return event_item_cards(db, account, profile, event, count, oos, preview).await,
        _ => Vec::new(),
    };
    let cards: Vec<Value> = products.iter().take(count as usize).map(card).collect();
    if let Some(k) = shared_key {
        shared_put(k, cards.clone());
    }
    Ok(cards)
}

/// This person's recently viewed products. A verified product shows as the catalog has it; an
/// unverified one as their own latest view of it described it.
pub async fn recently_viewed_cards(db: &PgPool, account: &Account, profile_id: Uuid, count: i64, oos: bool) -> AppResult<Vec<Value>> {
    let rows = recs::recently_viewed(db, account.id, profile_id, count, oos).await?;
    let own: Vec<Uuid> = rows.iter().filter(|(p, _)| !p.verified).map(|r| r.1).collect();
    let events: Vec<(Uuid, String, Value)> = if own.is_empty() {
        Vec::new()
    } else {
        sqlx::query_as("SELECT e.id, m.name, e.properties FROM events e JOIN metrics m ON m.id = e.metric_id WHERE e.id = ANY($1) AND e.profile_id = $2")
            .bind(&own)
            .bind(profile_id)
            .fetch_all(db)
            .await?
    };
    let hosts = super::learn::hosts_of(account.website.as_deref(), &account.verified_domains);
    Ok(rows
        .iter()
        .filter_map(|(p, ev)| {
            if p.verified {
                return Some(card(p));
            }
            let (_, metric, props) = events.iter().find(|e| e.0 == *ev)?;
            let items = super::learn::event_items(metric, props);
            let item = items.iter().find(|i| super::learn::text(i, "product_id").as_deref() == Some(p.external_id.as_str())).or(items.first())?;
            let c = super::learn::raw_card(item, props, &hosts);
            c["title"].is_string().then_some(c)
        })
        .collect())
}

/// The triggering event's items. A preview has no event, so it uses this person's latest cart
/// or checkout (else their latest order or product view) instead.
///
/// A verified catalog match shows with the catalog's current price, image and stock (a variant's
/// own when the item names one); an unverified one as the event described it; one the store has
/// unpublished doesn't show.
async fn event_item_cards(db: &PgPool, account: &Account, profile: Option<&Profile>, event: Option<&Value>, count: i64, oos: bool, preview: bool) -> AppResult<Vec<Value>> {
    let account_id = account.id;
    let sample;
    let event = match (event.filter(|e| e.get("properties").is_some()), profile) {
        (Some(e), _) => e,
        (None, Some(p)) if preview => {
            let row: Option<(String, Value)> = sqlx::query_as(
                "SELECT m.name, e.properties FROM events e JOIN metrics m ON m.id = e.metric_id
                 WHERE e.profile_id = $1 AND m.name IN ('Started Checkout', 'Added to Cart', 'Placed Order', 'Viewed Product')
                 ORDER BY (m.name IN ('Started Checkout', 'Added to Cart')) DESC, e.occurred_at DESC LIMIT 1",
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
    let hosts = super::learn::hosts_of(account.website.as_deref(), &account.verified_domains);
    let items = super::learn::event_cards(db, account_id, &hosts, event).await?;
    let ids: Vec<Uuid> = items.iter().filter_map(|(m, _)| m.map(|m| m.0)).collect();
    let vids: Vec<Uuid> = items.iter().filter_map(|(m, _)| m.and_then(|m| m.1)).collect();
    let known = recs::products_by_ids_all(db, account_id, &ids).await?;
    let variants = store::variants_by_ids(db, &vids).await?;
    let mut out = Vec::new();
    for (m, raw) in items {
        let c = match m.and_then(|(pid, vid)| known.get(&pid).map(|p| (p, vid))) {
            Some((p, _)) if !p.published => continue,
            Some((p, vid)) if p.verified => {
                // A learned product's variants hold only what browsers said: show the product's own (verified) details.
                let v = vid.filter(|_| p.source != "events").and_then(|vid| variants.iter().find(|(pid, v)| *pid == p.id && v.id == vid)).map(|x| &x.1);
                let mut c = match v {
                    Some(v) => store::variant_card(p, v),
                    None => card(p),
                };
                // Keep the event's quantity.
                c["quantity"] = raw["quantity"].clone();
                c
            }
            _ if raw["title"].is_string() => raw,
            _ => continue,
        };
        if oos && c["in_stock"] == json!(false) {
            continue;
        }
        if !c["id"].is_null() && out.iter().any(|x: &Value| x["id"] == c["id"] && x["variant_id"] == c["variant_id"]) {
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
pub async fn resolve_block(db: &PgPool, account: &Account, profile: Option<&Profile>, event: Option<&Value>, block: &Value, preview: bool) -> AppResult<Vec<Value>> {
    let count = block.get("count").and_then(|c| c.as_i64()).unwrap_or(4).clamp(1, MAX_COUNT);
    let hide_oos = block.get("hide_out_of_stock").and_then(|v| v.as_bool()).unwrap_or(true);
    let source = s(block, "source").unwrap_or("best_sellers");
    let spec = Spec { source, block, count, hide_oos };
    let mut items = source_cards(db, account, profile, event, &spec, preview).await?;
    if items.is_empty() {
        if let Some(fb) = s(block, "fallback").filter(|f| *f != source && SOURCES.contains(f)) {
            items = source_cards(db, account, profile, event, &Spec { source: fb, block, count, hide_oos }, preview).await?;
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

/// Whether any profile on the account has a property called `name` (checked once every ten
/// minutes per account). Then the short template name stays the profile's.
async fn property_in_use(db: &PgPool, account_id: Uuid, name: &'static str) -> AppResult<bool> {
    static SEEN: Lazy<Mutex<HashMap<(Uuid, &'static str), (Instant, bool)>>> = Lazy::new(|| Mutex::new(HashMap::new()));
    if let Some(hit) = SEEN.lock().ok().and_then(|m| m.get(&(account_id, name)).filter(|(at, _)| at.elapsed() < NAME_CHECK_TTL).map(|(_, b)| *b)) {
        return Ok(hit);
    }
    let used: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM profiles WHERE account_id = $1 AND properties @? $2::jsonpath)")
        .bind(account_id)
        .bind(format!("$.\"{name}\""))
        .fetch_one(db)
        .await?;
    if let Ok(mut m) = SEEN.lock() {
        if m.len() > 10_000 {
            m.clear();
        }
        m.insert((account_id, name), (Instant::now(), used));
    }
    Ok(used)
}

/// Fill a message's product blocks and catalog variables for one recipient. Returns the content
/// to render (unchanged and unqueried when it uses no catalog data).
pub async fn personalize<'a>(db: &PgPool, account: &Account, profile: Option<&Profile>, event: Option<&Value>, content: &'a Value, ctx: &mut Value, preview: bool) -> AppResult<Cow<'a, Value>> {
    let account_id = account.id;
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
                let items = resolve_block(db, account, profile, event, b, preview).await?;
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
            let cards = products_by_keys(db, account_id, &ids).await?;
            let mut by_key = serde_json::Map::new();
            for c in cards {
                for k in [c["_key"].clone(), c["_uuid"].clone()] {
                    if let Some(k) = k.as_str().filter(|k| ids.iter().any(|i| i == k)) {
                        by_key.insert(k.to_string(), strip_keys(c.clone()));
                    }
                }
            }
            catalog.insert("products".into(), Value::Object(by_key));
        }
        for (name, source) in [("best_sellers", "best_sellers"), ("newest", "newest"), ("on_sale", "on_sale")] {
            if text.contains(&format!("catalog.{name}")) {
                let block = json!({});
                let cards = source_cards(db, account, None, None, &Spec { source, block: &block, count: LIST_SIZE, hide_oos: true }, preview).await?;
                catalog.insert(name.into(), Value::Array(cards));
            }
        }
        let catalog = Value::Object(catalog);
        if ctx.get("catalog").is_none() && !property_in_use(db, account_id, "catalog").await? {
            ctx["catalog"] = catalog.clone();
        }
        if ctx["organization"].is_object() {
            ctx["organization"]["catalog"] = catalog;
        }
    }
    if refs.recommendations {
        let picks: Vec<Value> = match profile {
            Some(p) => recs::for_person(db, account_id, p.id, LIST_SIZE).await?.iter().map(card).collect(),
            None => {
                let block = json!({});
                source_cards(db, account, None, None, &Spec { source: "best_sellers", block: &block, count: LIST_SIZE, hide_oos: true }, preview).await?
            }
        };
        let picks = Value::Array(picks);
        if ctx.get("recommendations").is_none() && !property_in_use(db, account_id, "recommendations").await? {
            ctx["recommendations"] = picks.clone();
        }
        if ctx["person"].is_object() {
            ctx["person"]["recommendations"] = picks;
        }
    }
    Ok(Cow::Owned(out))
}

/// Used by the dashboard's product picker and `get_product`.
pub fn cards(products: &[Product]) -> Vec<Value> {
    products.iter().map(store::card).collect()
}
