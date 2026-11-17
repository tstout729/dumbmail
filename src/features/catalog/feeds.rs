//! Product feeds fetched on a schedule: a JSON feed (including Shopify's public `/products.json`)
//! or a Google Shopping / Merchant Center XML feed (RSS 2.0 or Atom with the `g:` namespace).
//!
//! Fetching uses the same protections as webhooks and brand import: public addresses only, DNS
//! pinned per hop (and no proxy from the environment), at most 5 redirects re-checked hop by hop,
//! time limits and a size cap. Fetches are conditional (ETag / Last-Modified, sent only to the
//! address they came from), so an unchanged feed costs one small request.
//!
//! A feed has one sync at a time: a sync holds a lease on the feed row, and checks before saving
//! each batch that the feed's setup (address, format, field map, on/off) hasn't changed since it
//! started. A sync that finds it has changed stops without saving anything more.

use std::collections::HashMap;
use std::sync::Mutex;

use chrono::{DateTime, Utc};
use once_cell::sync::Lazy;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use sqlx::PgPool;
use uuid::Uuid;

use super::store::{self, Origin, ProductInput};
use crate::error::{AppError, AppResult};
use crate::state::AppState;

pub const MAX_FEED_BYTES: usize = 25 * 1024 * 1024;
pub const MAX_FEED_PRODUCTS: usize = 50_000;
/// Most XML nodes (elements, text, attributes) one feed may have.
const MAX_XML_NODES: u32 = 3_000_000;
/// Products saved per batch; the sync re-checks its lease and the feed's setup between batches.
const SYNC_CHUNK: usize = 200;
/// How long a sync's lease lasts without being renewed.
const LEASE_MINUTES: i32 = 15;
const MAX_FEEDS: i64 = 20;
const GOOGLE_NS: &str = "http://base.google.com/ns/1.0";

/// Fields a field map may set. `custom.<name>` puts any other feed field into custom fields.
pub const MAP_FIELDS: &[&str] = &[
    "items_path", "external_id", "title", "description", "url", "image_url", "images", "price", "sale_price", "compare_at_price", "currency",
    "available", "inventory", "categories", "tags", "brand", "vendor", "published", "group_id", "variant_title", "sku",
];

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct Feed {
    pub id: Uuid,
    pub name: String,
    pub url: String,
    pub format: String,
    pub field_map: Value,
    pub interval_hours: i32,
    pub enabled: bool,
    pub unpublish_missing: bool,
    pub next_sync_at: DateTime<Utc>,
    pub last_synced_at: Option<DateTime<Utc>>,
    pub last_status: Option<String>,
    pub last_error: Option<String>,
    pub last_counts: Value,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

const FEED_COLS: &str = "id, name, url, format, field_map, interval_hours, enabled, unpublish_missing, next_sync_at, last_synced_at, last_status, last_error, last_counts, created_at, updated_at";

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FeedInput {
    pub name: Option<String>,
    pub url: Option<String>,
    pub format: Option<String>,
    pub field_map: Option<Value>,
    pub interval_hours: Option<i32>,
    pub enabled: Option<bool>,
    pub unpublish_missing: Option<bool>,
}

fn check_map(v: &Value) -> AppResult<Value> {
    let o = v.as_object().ok_or_else(|| AppError::bad("field_map must be an object of our field -> the feed's field"))?;
    if o.len() > 60 {
        return Err(AppError::bad("field_map can have at most 60 entries"));
    }
    let mut out = Map::new();
    for (k, val) in o {
        let ok = MAP_FIELDS.contains(&k.as_str()) || k.strip_prefix("custom.").is_some_and(|n| !n.is_empty() && n.len() <= 100);
        if !ok {
            return Err(AppError::bad(format!("field_map can't set {k:?}; use one of {} or custom.<name>", MAP_FIELDS.join(", "))));
        }
        let path = val.as_str().map(str::trim).unwrap_or("");
        if path.is_empty() {
            continue;
        }
        if path.len() > 200 {
            return Err(AppError::bad(format!("the feed field for {k} is too long")));
        }
        out.insert(k.clone(), json!(path));
    }
    Ok(Value::Object(out))
}

fn check_format(f: &str) -> AppResult<()> {
    if matches!(f, "json" | "google_xml") { Ok(()) } else { Err(AppError::bad("format must be json or google_xml")) }
}

fn check_url(u: &str) -> AppResult<String> {
    let u = u.trim();
    match url::Url::parse(u) {
        Ok(p) if !p.username().is_empty() || p.password().is_some() => {
            Err(AppError::bad("the feed address can't hold a username or password; use a feed link that works without signing in"))
        }
        Ok(p) if matches!(p.scheme(), "http" | "https") && p.host_str().is_some() && u.len() <= 2000 => Ok(p.to_string()),
        _ => Err(AppError::bad("the feed address must be an http(s) URL")),
    }
}

pub async fn list(db: &PgPool, account_id: Uuid) -> AppResult<Vec<Value>> {
    let q = format!("SELECT {FEED_COLS} FROM catalog_feeds WHERE account_id = $1 ORDER BY created_at");
    let rows: Vec<Feed> = sqlx::query_as(&q).bind(account_id).fetch_all(db).await?;
    let mut out = Vec::new();
    for f in rows {
        let n: i64 = sqlx::query_scalar("SELECT count(*) FROM catalog_products WHERE feed_id = $1").bind(f.id).fetch_one(db).await?;
        let queued: bool = sqlx::query_scalar(
            "SELECT (sync_queued_at > now() - interval '1 hour') IS TRUE OR (sync_lease_until > now()) IS TRUE FROM catalog_feeds WHERE id = $1",
        )
        .bind(f.id)
        .fetch_one(db)
        .await?;
        let mut v = serde_json::to_value(&f).map_err(|e| AppError::Internal(e.into()))?;
        v["products"] = json!(n);
        v["syncing"] = json!(queued);
        out.push(v);
    }
    Ok(out)
}

pub async fn get(db: &PgPool, account_id: Uuid, id: Uuid) -> AppResult<Feed> {
    let q = format!("SELECT {FEED_COLS} FROM catalog_feeds WHERE id = $1 AND account_id = $2");
    sqlx::query_as(&q).bind(id).bind(account_id).fetch_optional(db).await?.ok_or_else(|| AppError::not_found("feed"))
}

/// Add a feed and queue its first sync.
pub async fn create(db: &PgPool, account_id: Uuid, b: FeedInput) -> AppResult<Feed> {
    let url = check_url(b.url.as_deref().ok_or_else(|| AppError::bad("a feed needs a url"))?)?;
    let format = b.format.clone().unwrap_or_else(|| if url.to_lowercase().contains(".xml") || url.contains("rss") { "google_xml".into() } else { "json".into() });
    check_format(&format)?;
    let name = b.name.as_deref().map(str::trim).filter(|n| !n.is_empty()).unwrap_or("Product feed").chars().take(120).collect::<String>();
    let map = check_map(b.field_map.as_ref().unwrap_or(&json!({})))?;
    let interval = b.interval_hours.unwrap_or(6);
    if !(1..=168).contains(&interval) {
        return Err(AppError::bad("sync every 1 to 168 hours"));
    }
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM catalog_feeds WHERE account_id = $1").bind(account_id).fetch_one(db).await?;
    if n >= MAX_FEEDS {
        return Err(AppError::bad(format!("an account can have at most {MAX_FEEDS} feeds")));
    }
    let id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO catalog_feeds (id, account_id, name, url, format, field_map, interval_hours, enabled, unpublish_missing) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
    )
    .bind(id)
    .bind(account_id)
    .bind(&name)
    .bind(&url)
    .bind(&format)
    .bind(&map)
    .bind(interval)
    .bind(b.enabled.unwrap_or(true))
    .bind(b.unpublish_missing.unwrap_or(true))
    .execute(db)
    .await?;
    if b.enabled.unwrap_or(true) {
        queue_sync(db, id, true).await?;
    }
    get(db, account_id, id).await
}

pub async fn update(db: &PgPool, account_id: Uuid, id: Uuid, b: FeedInput) -> AppResult<Feed> {
    let cur = get(db, account_id, id).await?;
    let url = match &b.url {
        Some(u) => check_url(u)?,
        None => cur.url.clone(),
    };
    let format = b.format.clone().unwrap_or(cur.format.clone());
    check_format(&format)?;
    let map = match &b.field_map {
        Some(m) => check_map(m)?,
        None => cur.field_map.clone(),
    };
    let interval = b.interval_hours.unwrap_or(cur.interval_hours);
    if !(1..=168).contains(&interval) {
        return Err(AppError::bad("sync every 1 to 168 hours"));
    }
    let name = b.name.as_deref().map(str::trim).filter(|n| !n.is_empty()).map(|n| n.chars().take(120).collect()).unwrap_or(cur.name.clone());
    // A new address or mapping needs a full fetch: forget what the old one said. Any change to what
    // a sync reads (or turning the feed off) makes a sync already running stop without saving more.
    let reset = url != cur.url || format != cur.format || map != cur.field_map;
    let enabled = b.enabled.unwrap_or(cur.enabled);
    let bump = reset || enabled != cur.enabled;
    sqlx::query(
        "UPDATE catalog_feeds SET name = $3, url = $4, format = $5, field_map = $6, interval_hours = $7, enabled = $8, unpublish_missing = $9,
            etag = CASE WHEN $10 THEN NULL ELSE etag END, last_modified = CASE WHEN $10 THEN NULL ELSE last_modified END,
            validator_url = CASE WHEN $10 THEN NULL ELSE validator_url END,
            config_version = config_version + CASE WHEN $11 THEN 1 ELSE 0 END, updated_at = now()
         WHERE id = $1 AND account_id = $2",
    )
    .bind(id)
    .bind(account_id)
    .bind(&name)
    .bind(&url)
    .bind(&format)
    .bind(&map)
    .bind(interval)
    .bind(enabled)
    .bind(b.unpublish_missing.unwrap_or(cur.unpublish_missing))
    .bind(reset)
    .bind(bump)
    .execute(db)
    .await?;
    if reset && enabled {
        queue_sync(db, id, true).await?;
    }
    get(db, account_id, id).await
}

/// Remove a feed. Its products stay in the catalog (and stop being updated).
pub async fn delete(db: &PgPool, account_id: Uuid, id: Uuid) -> AppResult<Feed> {
    let f = get(db, account_id, id).await?;
    sqlx::query("DELETE FROM catalog_feeds WHERE id = $1 AND account_id = $2").bind(id).bind(account_id).execute(db).await?;
    Ok(f)
}

/// Queue a sync unless one is already waiting (then a `full` request makes that one full). `full`
/// ignores the saved ETag. Marking the feed and queuing the job is one statement, so two requests
/// can't both queue one. A sync that's running doesn't count: the queued one runs after it.
pub async fn queue_sync(db: &PgPool, feed_id: Uuid, full: bool) -> AppResult<bool> {
    let queued: Option<i64> = sqlx::query_scalar(
        "WITH f AS (UPDATE catalog_feeds SET sync_queued_at = now(), sync_full = $2
                    WHERE id = $1 AND enabled AND (sync_queued_at IS NULL OR sync_queued_at < now() - interval '1 hour') RETURNING id)
         INSERT INTO jobs (kind, payload) SELECT 'catalog_feed_sync', jsonb_build_object('feed_id', id) FROM f RETURNING id",
    )
    .bind(feed_id)
    .bind(full)
    .fetch_optional(db)
    .await?;
    if queued.is_none() && full {
        sqlx::query("UPDATE catalog_feeds SET sync_full = true WHERE id = $1 AND sync_queued_at IS NOT NULL").bind(feed_id).execute(db).await?;
    }
    Ok(queued.is_some())
}

/// Scheduler: queue the feeds that are due.
pub async fn queue_due(db: &PgPool) -> anyhow::Result<()> {
    let due: Vec<Uuid> = sqlx::query_scalar(
        "UPDATE catalog_feeds SET next_sync_at = now() + make_interval(hours => interval_hours)
         WHERE id IN (SELECT id FROM catalog_feeds WHERE enabled AND next_sync_at <= now() ORDER BY next_sync_at LIMIT 50) RETURNING id",
    )
    .fetch_all(db)
    .await?;
    for id in due {
        queue_sync(db, id, false).await.map_err(|e| anyhow::anyhow!(e.public_message()))?;
    }
    Ok(())
}

// ---------- Fetching ----------

/// Loopback ports the test suite's stand-in feed servers listen on. Only tests add to this; the
/// server never does, so real feeds can't reach private addresses.
#[doc(hidden)]
pub static TEST_LOOPBACK_PORTS: Lazy<Mutex<Vec<u16>>> = Lazy::new(|| Mutex::new(Vec::new()));

async fn vet(u: &str, hosts: Option<&[String]>) -> AppResult<(String, std::net::SocketAddr)> {
    if let Some(h) = hosts {
        if !super::learn::on_site(h, u) {
            return Err(AppError::bad("the page redirected away from the store's own site"));
        }
    }
    if let Ok(p) = url::Url::parse(u) {
        let port = p.port_or_known_default().unwrap_or(0);
        if p.host_str() == Some("127.0.0.1") && TEST_LOOPBACK_PORTS.lock().map(|l| l.contains(&port)).unwrap_or(false) {
            return Ok(("127.0.0.1".into(), std::net::SocketAddr::from(([127, 0, 0, 1], port))));
        }
    }
    crate::flows::check_webhook_url(u).await.map_err(|_| AppError::bad("the feed address must be a public website (not a private or local network)"))
}

pub enum Fetched {
    NotModified,
    /// `url` is where the body came from after redirects; `etag` and `last_modified` belong to it.
    Body { bytes: Vec<u8>, url: String, etag: Option<String>, last_modified: Option<String> },
}

/// Saved ETag / Last-Modified and the address they came from.
#[derive(Default, Clone, Copy)]
pub struct Validators<'a> {
    pub url: Option<&'a str>,
    pub etag: Option<&'a str>,
    pub last_modified: Option<&'a str>,
}

/// Fetch `raw` (at most `max` bytes). Validators are sent only to the address they came from.
/// With `hosts`, every hop must stay on those hosts (a store's own site).
pub async fn fetch(raw: &str, v: Option<Validators<'_>>, max: usize, hosts: Option<&[String]>) -> AppResult<Fetched> {
    let mut url = raw.to_string();
    for _ in 0..6 {
        let (host, addr) = vet(&url, hosts).await?;
        let client = reqwest::Client::builder()
            .resolve(&host, addr)
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(std::time::Duration::from_secs(10))
            .timeout(std::time::Duration::from_secs(90))
            .user_agent("DumbMail-Feed/1.0 (+https://dumbmail.ai)")
            .build()
            .map_err(|e| AppError::Internal(e.into()))?;
        let mut req = client.get(&url).header("accept", "application/json, application/rss+xml, application/atom+xml, application/xml, text/xml;q=0.9, text/html;q=0.8, */*;q=0.5");
        if let Some(v) = v.filter(|v| v.url == Some(url.as_str())) {
            if let Some(e) = v.etag {
                req = req.header("if-none-match", e);
            }
            if let Some(m) = v.last_modified {
                req = req.header("if-modified-since", m);
            }
        }
        let res = req.send().await.map_err(|e| AppError::bad(format!("couldn't reach the feed: {}", e.without_url())))?;
        let status = res.status();
        if status == reqwest::StatusCode::NOT_MODIFIED {
            return Ok(Fetched::NotModified);
        }
        if status.is_redirection() {
            let loc = res.headers().get("location").and_then(|v| v.to_str().ok()).unwrap_or_default();
            url = url::Url::parse(&url).and_then(|b| b.join(loc)).map(|u| u.to_string()).map_err(|_| AppError::bad("the feed redirected somewhere invalid"))?;
            continue;
        }
        if !status.is_success() {
            return Err(AppError::bad(format!("the feed returned HTTP {}", status.as_u16())));
        }
        let too_big = || AppError::bad(if max == MAX_FEED_BYTES { "the feed is larger than 25 MB; split it into smaller feeds".to_string() } else { format!("the page is larger than {} MB", max / (1024 * 1024)) });
        if res.content_length().is_some_and(|n| n as usize > max) {
            return Err(too_big());
        }
        let header = |k: &str| res.headers().get(k).and_then(|v| v.to_str().ok()).map(|s| s.chars().take(300).collect::<String>());
        let (etag, last_modified) = (header("etag"), header("last-modified"));
        let mut bytes = Vec::new();
        let mut stream = res;
        while let Some(chunk) = stream.chunk().await.map_err(|e| AppError::bad(format!("couldn't read the feed: {}", e.without_url())))? {
            bytes.extend_from_slice(&chunk);
            if bytes.len() > max {
                return Err(too_big());
            }
        }
        return Ok(Fetched::Body { bytes, url, etag, last_modified });
    }
    Err(AppError::bad("the feed redirects too many times"))
}

// ---------- Parsing ----------

fn path<'a>(v: &'a Value, p: &str) -> Option<&'a Value> {
    let mut cur = v;
    for part in p.split('.') {
        cur = match cur {
            Value::Object(o) => o.get(part)?,
            Value::Array(a) => a.get(part.parse::<usize>().ok()?)?,
            _ => return None,
        };
    }
    (!cur.is_null()).then_some(cur)
}

/// An image from a URL string or an object with `src` / `url`.
fn image(v: &Value) -> Option<Value> {
    match v {
        Value::String(s) => Some(json!(s)),
        Value::Object(o) => o.get("src").or_else(|| o.get("url")).filter(|s| s.is_string()).cloned(),
        _ => None,
    }
}

/// The first of these fields the record has, or the mapped one.
fn pick<'a>(rec: &'a Value, map: &Value, field: &str, defaults: &[&str]) -> Option<&'a Value> {
    match map.get(field).and_then(|m| m.as_str()) {
        Some(p) => path(rec, p),
        None => defaults.iter().find_map(|d| path(rec, d)),
    }
}

fn json_record(rec: &Value, map: &Value, origin: &str) -> Value {
    let mut out = Map::new();
    let mut put = |k: &str, v: Option<Value>| {
        if let Some(v) = v.filter(|v| !v.is_null()) {
            out.insert(k.into(), v);
        }
    };
    put("external_id", pick(rec, map, "external_id", &["id", "product_id", "sku"]).cloned());
    put("title", pick(rec, map, "title", &["title", "name"]).cloned());
    put("description", pick(rec, map, "description", &["description", "body_html"]).cloned());
    let url = pick(rec, map, "url", &["url", "link", "permalink"]).cloned().or_else(|| {
        // Shopify's /products.json has a handle instead of a link.
        rec.get("handle").and_then(|h| h.as_str()).filter(|_| !origin.is_empty()).map(|h| json!(format!("{origin}/products/{h}")))
    });
    put("url", url);
    let images: Vec<Value> = pick(rec, map, "images", &["images"]).and_then(|v| v.as_array()).map(|a| a.iter().filter_map(image).collect()).unwrap_or_default();
    put("image_url", pick(rec, map, "image_url", &["image_url", "image", "featured_image", "thumbnail"]).and_then(image).or_else(|| images.first().cloned()));
    if !images.is_empty() {
        put("images", Some(Value::Array(images)));
    }
    let price = pick(rec, map, "price", &["price"]).cloned();
    let sale = pick(rec, map, "sale_price", &["sale_price"]).cloned();
    match (price, sale) {
        (Some(p), Some(s)) => {
            put("price", Some(s));
            put("compare_at_price", Some(p));
        }
        (p, None) => {
            put("price", p);
            put("compare_at_price", pick(rec, map, "compare_at_price", &["compare_at_price", "regular_price", "original_price"]).cloned());
        }
        (None, s) => put("price", s),
    }
    put("currency", pick(rec, map, "currency", &["currency", "price_currency"]).cloned());
    put("available", pick(rec, map, "available", &["available", "availability", "in_stock"]).cloned());
    put("inventory", pick(rec, map, "inventory", &["inventory", "inventory_quantity", "quantity", "stock"]).cloned());
    put("categories", pick(rec, map, "categories", &["categories", "category", "collections", "product_type"]).cloned());
    put("tags", pick(rec, map, "tags", &["tags"]).cloned());
    put("brand", pick(rec, map, "brand", &["brand"]).cloned());
    put("vendor", pick(rec, map, "vendor", &["vendor"]).cloned());
    put("published", pick(rec, map, "published", &["published"]).cloned());
    let mut custom = Map::new();
    if let Some(m) = map.as_object() {
        for (k, p) in m {
            if let (Some(name), Some(p)) = (k.strip_prefix("custom."), p.as_str()) {
                if let Some(v) = path(rec, p) {
                    custom.insert(name.to_string(), v.clone());
                }
            }
        }
    }
    if !custom.is_empty() {
        put("custom", Some(Value::Object(custom)));
    }
    if let Some(vs) = rec.get("variants").and_then(|v| v.as_array()) {
        let variants: Vec<Value> = vs
            .iter()
            .filter(|v| v.is_object())
            .map(|v| {
                let mut o = Map::new();
                for (to, from) in [("external_id", &["id", "sku"][..]), ("title", &["title", "name"]), ("sku", &["sku"]), ("price", &["price"]), ("compare_at_price", &["compare_at_price"]), ("available", &["available", "in_stock"]), ("inventory", &["inventory", "inventory_quantity", "quantity"])] {
                    if let Some(x) = from.iter().find_map(|f| path(v, f)) {
                        o.insert(to.into(), x.clone());
                    }
                }
                if let Some(i) = ["image_url", "image", "featured_image"].iter().find_map(|f| path(v, f)).and_then(image) {
                    o.insert("image_url".into(), i);
                }
                if o.get("title").and_then(|t| t.as_str()) == Some("Default Title") {
                    o.remove("title");
                }
                Value::Object(o)
            })
            .collect();
        if !variants.is_empty() {
            out.insert("variants".into(), Value::Array(variants));
        }
    }
    Value::Object(out)
}

fn to_input(v: Value, n: usize) -> Result<ProductInput, String> {
    serde_json::from_value(v).map_err(|e| format!("item {n}: {e}"))
}

pub fn parse_json(body: &[u8], map: &Value, feed_url: &str) -> Result<(Vec<ProductInput>, Vec<String>), String> {
    let v: Value = serde_json::from_slice(body).map_err(|e| format!("the feed isn't valid JSON ({e})"))?;
    let records = match (&v, map.get("items_path").and_then(|p| p.as_str())) {
        (_, Some(p)) => path(&v, p).and_then(|x| x.as_array()).ok_or_else(|| format!("the feed has no list at {p:?}"))?,
        (Value::Array(a), None) => a,
        (Value::Object(o), None) => ["products", "items", "data", "entries", "results"]
            .iter()
            .find_map(|k| o.get(*k).and_then(|x| x.as_array()))
            .ok_or("couldn't find the list of products; set items_path in the field map")?,
        _ => return Err("the feed should be a list of products or an object holding one".into()),
    };
    let origin = url::Url::parse(feed_url).ok().map(|u| u.origin().ascii_serialization()).unwrap_or_default();
    let mut out = Vec::new();
    let mut errors = Vec::new();
    for (i, r) in records.iter().take(MAX_FEED_PRODUCTS).enumerate() {
        match to_input(json_record(r, map, &origin), i + 1) {
            Ok(p) if !p.external_id.trim().is_empty() => out.push(p),
            Ok(_) => errors.push(format!("item {}: no id", i + 1)),
            Err(e) => errors.push(e),
        }
    }
    if records.len() > MAX_FEED_PRODUCTS {
        errors.push(format!("only the first {MAX_FEED_PRODUCTS} products were read"));
    }
    Ok((out, errors))
}

/// One feed item's fields: Google's as `g:name`, others by their plain name. Atom links use `href`.
fn xml_fields(node: roxmltree::Node<'_, '_>) -> HashMap<String, Vec<String>> {
    let mut out: HashMap<String, Vec<String>> = HashMap::new();
    for c in node.children().filter(|c| c.is_element()) {
        let tag = c.tag_name();
        let key = if tag.namespace() == Some(GOOGLE_NS) { format!("g:{}", tag.name()) } else { tag.name().to_string() };
        let text = if tag.name() == "link" && tag.namespace() != Some(GOOGLE_NS) && c.attribute("href").is_some() {
            c.attribute("href").unwrap_or_default().to_string()
        } else if c.children().any(|x| x.is_element()) {
            // Nested values such as <g:shipping>: keep the text of the parts.
            c.descendants().filter(|d| d.is_text()).filter_map(|d| d.text()).map(str::trim).filter(|t| !t.is_empty()).collect::<Vec<_>>().join(" ")
        } else {
            c.text().unwrap_or("").trim().to_string()
        };
        if !text.is_empty() {
            out.entry(key).or_default().push(text.chars().take(20_000).collect());
        }
    }
    out
}

fn first(f: &HashMap<String, Vec<String>>, map: &Value, field: &str, defaults: &[&str]) -> Option<String> {
    match map.get(field).and_then(|m| m.as_str()) {
        Some(k) => f.get(k).and_then(|v| v.first()).cloned(),
        None => defaults.iter().find_map(|d| f.get(*d).and_then(|v| v.first()).cloned()),
    }
}

/// The 3-letter currency at the end of a Google price ("15.00 USD").
fn currency_of(price: &str) -> Option<String> {
    price.split_whitespace().last().filter(|c| c.len() == 3 && c.chars().all(|x| x.is_ascii_alphabetic())).map(|c| c.to_ascii_uppercase())
}

pub fn parse_google_xml(body: &[u8], map: &Value) -> Result<(Vec<ProductInput>, Vec<String>), String> {
    parse_xml(body, map, MAX_XML_NODES)
}

fn parse_xml(body: &[u8], map: &Value, nodes_limit: u32) -> Result<(Vec<ProductInput>, Vec<String>), String> {
    let text = String::from_utf8_lossy(body);
    let opts = roxmltree::ParsingOptions { allow_dtd: false, nodes_limit, ..Default::default() };
    let doc = roxmltree::Document::parse_with_options(&text, opts).map_err(|e| match e {
        roxmltree::Error::NodesLimitReached => format!("the feed has more than {nodes_limit} XML nodes; split it into smaller feeds"),
        e => format!("the feed isn't valid XML ({e})"),
    })?;
    let root = doc.root_element();
    let items: Vec<roxmltree::Node<'_, '_>> = match root.tag_name().name() {
        "rss" => root.children().filter(|c| c.has_tag_name("channel")).flat_map(|ch| ch.children().filter(|c| c.has_tag_name("item"))).collect(),
        "feed" => root.children().filter(|c| c.has_tag_name("entry")).collect(),
        other => return Err(format!("expected an RSS (<rss>) or Atom (<feed>) document, got <{other}>")),
    };
    let mut errors = Vec::new();
    // Items sharing g:item_group_id are variants of one product.
    let mut groups: Vec<(String, Vec<Value>)> = Vec::new();
    let mut group_at: HashMap<String, usize> = HashMap::new();
    let mut products: Vec<Value> = Vec::new();
    for (i, node) in items.iter().take(MAX_FEED_PRODUCTS).enumerate() {
        let f = xml_fields(*node);
        let Some(id) = first(&f, map, "external_id", &["g:id", "id"]) else {
            errors.push(format!("item {}: no g:id", i + 1));
            continue;
        };
        let price_raw = first(&f, map, "price", &["g:price"]);
        let sale_raw = first(&f, map, "sale_price", &["g:sale_price"]);
        let currency = first(&f, map, "currency", &[]).or_else(|| price_raw.as_deref().and_then(currency_of)).or_else(|| sale_raw.as_deref().and_then(currency_of));
        let (price, compare) = match (&price_raw, &sale_raw) {
            (Some(p), Some(s)) => (Some(s.clone()), Some(p.clone())),
            (p, _) => (p.clone(), first(&f, map, "compare_at_price", &[])),
        };
        let options: Map<String, Value> = ["color", "size", "material", "pattern", "age_group", "gender"]
            .iter()
            .filter_map(|o| f.get(&format!("g:{o}")).and_then(|v| v.first()).map(|v| (o.to_string(), json!(v))))
            .collect();
        let variant_title = first(&f, map, "variant_title", &[]).or_else(|| {
            let parts: Vec<&str> = options.values().filter_map(|v| v.as_str()).collect();
            (!parts.is_empty()).then(|| parts.join(" / "))
        });
        let tags: Vec<String> = match map.get("tags").and_then(|m| m.as_str()) {
            Some(k) => f.get(k).cloned().unwrap_or_default(),
            None => (0..5).filter_map(|n| f.get(&format!("g:custom_label_{n}")).and_then(|v| v.first()).cloned()).collect(),
        };
        let mut custom = Map::new();
        if let Some(m) = map.as_object() {
            for (k, p) in m {
                if let (Some(name), Some(p)) = (k.strip_prefix("custom."), p.as_str()) {
                    if let Some(v) = f.get(p).and_then(|v| v.first()) {
                        custom.insert(name.to_string(), json!(v));
                    }
                }
            }
        }
        let rec = json!({
            "external_id": id,
            "title": first(&f, map, "title", &["g:title", "title"]),
            "description": first(&f, map, "description", &["g:description", "description", "summary", "content"]),
            "url": first(&f, map, "url", &["g:link", "link"]),
            "image_url": first(&f, map, "image_url", &["g:image_link"]),
            "images": match map.get("images").and_then(|m| m.as_str()) { Some(k) => f.get(k).cloned(), None => f.get("g:additional_image_link").cloned() },
            "price": price, "compare_at_price": compare, "currency": currency,
            "available": first(&f, map, "available", &["g:availability"]),
            "inventory": first(&f, map, "inventory", &["g:quantity"]),
            "categories": first(&f, map, "categories", &["g:product_type"]),
            "tags": tags,
            "brand": first(&f, map, "brand", &["g:brand"]),
            "vendor": first(&f, map, "vendor", &[]),
            "sku": first(&f, map, "sku", &["g:mpn", "g:gtin"]),
            "options": options,
            "variant_title": variant_title,
            "custom": if custom.is_empty() { Value::Null } else { Value::Object(custom) },
        });
        match first(&f, map, "group_id", &["g:item_group_id"]) {
            Some(g) => match group_at.get(&g) {
                Some(&i) => groups[i].1.push(rec),
                None => {
                    group_at.insert(g.clone(), groups.len());
                    groups.push((g, vec![rec]));
                }
            },
            None => products.push(rec),
        }
    }
    if items.len() > MAX_FEED_PRODUCTS {
        errors.push(format!("only the first {MAX_FEED_PRODUCTS} items were read"));
    }
    let strip = |mut v: Value| {
        if let Some(o) = v.as_object_mut() {
            for k in ["sku", "options", "variant_title"] {
                o.remove(k);
            }
            o.retain(|_, x| !x.is_null());
        }
        v
    };
    let mut out = Vec::new();
    for (n, rec) in products.into_iter().enumerate() {
        match to_input(strip(rec), n + 1) {
            Ok(p) => out.push(p),
            Err(e) => errors.push(e),
        }
    }
    for (gid, items) in groups {
        let mut base = items[0].clone();
        let variants: Vec<Value> = items
            .iter()
            .map(|r| {
                json!({"external_id": r["external_id"], "title": r["variant_title"], "sku": r["sku"], "price": r["price"], "compare_at_price": r["compare_at_price"],
                       "available": r["available"], "inventory": r["inventory"], "image_url": r["image_url"], "options": r["options"]})
            })
            .map(|mut v| {
                if let Some(o) = v.as_object_mut() {
                    o.retain(|_, x| !x.is_null());
                }
                v
            })
            .collect();
        base["external_id"] = json!(gid);
        base["variants"] = json!(variants);
        // The product's price comes from its cheapest variant.
        if let Some(o) = base.as_object_mut() {
            for k in ["price", "compare_at_price", "available", "inventory"] {
                o.remove(k);
            }
        }
        match to_input(strip(base), out.len() + 1) {
            Ok(p) => out.push(p),
            Err(e) => errors.push(format!("group {gid}: {e}")),
        }
    }
    Ok((out, errors))
}

// ---------- CSV ----------

pub const MAX_CSV_ROWS: usize = 50_000;

fn csv_role(h: &str) -> Option<&'static str> {
    let k: String = h.to_lowercase().chars().filter(|c| c.is_alphanumeric()).collect();
    Some(match k.as_str() {
        "id" | "externalid" | "productid" | "itemid" => "external_id",
        "title" | "name" | "producttitle" | "productname" => "title",
        "description" | "bodyhtml" | "body" => "description",
        "url" | "link" | "producturl" | "productlink" => "url",
        "image" | "imageurl" | "imagesrc" | "imagelink" | "featuredimage" => "image_url",
        "price" | "variantprice" => "price",
        "compareatprice" | "compareprice" | "originalprice" | "regularprice" | "variantcompareatprice" => "compare_at_price",
        "currency" => "currency",
        "inventory" | "quantity" | "stock" | "inventoryquantity" | "qty" | "variantinventoryqty" => "inventory",
        "available" | "availability" | "instock" => "available",
        "categories" | "category" | "collections" | "collection" | "producttype" | "type" => "categories",
        "tags" => "tags",
        "brand" => "brand",
        "vendor" => "vendor",
        "published" | "status" | "visible" => "published",
        "variantid" => "variant_id",
        "sku" | "variantsku" => "sku",
        "varianttitle" | "variant" | "option" | "option1value" => "variant_title",
        _ => return None,
    })
}

/// Products from a CSV with a header row. Rows that share an id with a `variant_id` column become
/// that product's variants. Unknown columns become custom fields.
pub fn parse_csv(text: &str) -> Result<(Vec<ProductInput>, Vec<String>), String> {
    let mut rdr = csv::ReaderBuilder::new().flexible(true).trim(csv::Trim::All).from_reader(text.as_bytes());
    let headers: Vec<String> = rdr.headers().map_err(|e| format!("couldn't read the CSV header: {e}"))?.iter().map(|h| h.trim_start_matches('\u{feff}').to_string()).collect();
    let roles: Vec<Option<&str>> = headers.iter().map(|h| csv_role(h)).collect();
    if !roles.contains(&Some("external_id")) {
        return Err("the CSV needs an id column (id, external_id or product_id)".into());
    }
    let has_variants = roles.contains(&Some("variant_id"));
    let mut order: Vec<String> = Vec::new();
    let mut products: HashMap<String, Map<String, Value>> = HashMap::new();
    let mut errors = Vec::new();
    for (i, rec) in rdr.records().enumerate() {
        if i >= MAX_CSV_ROWS {
            errors.push(format!("stopped after {MAX_CSV_ROWS} rows; split the file"));
            break;
        }
        let rec = match rec {
            Ok(r) => r,
            Err(e) => {
                errors.push(format!("line {}: {e}", i + 2));
                continue;
            }
        };
        let mut row = Map::new();
        let mut custom = Map::new();
        for (j, val) in rec.iter().enumerate() {
            if val.is_empty() {
                continue;
            }
            match roles.get(j).copied().flatten() {
                Some(role) => {
                    row.insert(role.into(), json!(val));
                }
                None => {
                    if let Some(h) = headers.get(j).filter(|h| !h.is_empty()) {
                        custom.insert(h.chars().take(100).collect(), json!(val));
                    }
                }
            }
        }
        let Some(id) = row.get("external_id").and_then(|v| v.as_str()).map(String::from) else {
            errors.push(format!("line {}: no id", i + 2));
            continue;
        };
        let entry = products.entry(id.clone()).or_insert_with(|| {
            order.push(id.clone());
            Map::new()
        });
        if has_variants {
            if let Some(vid) = row.get("variant_id").cloned() {
                let mut v = Map::new();
                v.insert("external_id".into(), vid);
                for (to, from) in [("title", "variant_title"), ("sku", "sku"), ("price", "price"), ("compare_at_price", "compare_at_price"), ("inventory", "inventory"), ("available", "available"), ("image_url", "image_url")] {
                    if let Some(x) = row.get(from) {
                        v.insert(to.into(), x.clone());
                    }
                }
                if let Some(a) = entry.entry("variants").or_insert_with(|| json!([])).as_array_mut() {
                    a.push(Value::Object(v));
                }
                // The product's own fields come from its first row; variant prices set the product's price.
                for k in ["price", "compare_at_price", "inventory", "available", "variant_id", "sku", "variant_title"] {
                    row.remove(k);
                }
            }
        }
        for (k, v) in row {
            if k != "variant_id" && k != "variant_title" && k != "sku" {
                entry.entry(k).or_insert(v);
            }
        }
        if !custom.is_empty() {
            let c = entry.entry("custom").or_insert_with(|| json!({}));
            if let Some(o) = c.as_object_mut() {
                for (k, v) in custom {
                    o.entry(k).or_insert(v);
                }
            }
        }
    }
    let mut out = Vec::new();
    for id in order {
        if let Some(m) = products.remove(&id) {
            match to_input(Value::Object(m), out.len() + 1) {
                Ok(p) => out.push(p),
                Err(e) => errors.push(format!("product {id}: {e}")),
            }
        }
    }
    Ok((out, errors))
}

// ---------- Sync ----------

#[derive(Deserialize)]
struct SyncJob {
    feed_id: Uuid,
}

/// What a sync read when it took the feed's lease.
#[derive(sqlx::FromRow)]
struct Claimed {
    account_id: Uuid,
    url: String,
    format: String,
    field_map: Value,
    etag: Option<String>,
    last_modified: Option<String>,
    validator_url: Option<String>,
    unpublish_missing: bool,
    config_version: i32,
    sync_full: bool,
}

/// The sync's lease and the feed setup it started with: every write it makes checks both.
struct Lease {
    feed_id: Uuid,
    token: Uuid,
    version: i32,
}

impl Lease {
    /// Still ours, and the feed's setup unchanged (renews the lease).
    async fn current(&self, db: &PgPool) -> sqlx::Result<bool> {
        Ok(sqlx::query(
            "UPDATE catalog_feeds SET sync_lease_until = now() + make_interval(mins => $4)
             WHERE id = $1 AND sync_lease = $2 AND config_version = $3 AND enabled",
        )
        .bind(self.feed_id)
        .bind(self.token)
        .bind(self.version)
        .bind(LEASE_MINUTES)
        .execute(db)
        .await?
        .rows_affected()
            == 1)
    }

    async fn release(&self, db: &PgPool) -> sqlx::Result<()> {
        sqlx::query("UPDATE catalog_feeds SET sync_lease = NULL, sync_lease_until = NULL WHERE id = $1 AND sync_lease = $2").bind(self.feed_id).bind(self.token).execute(db).await?;
        Ok(())
    }

    /// Record the outcome and let go of the feed, unless the setup changed meanwhile (then the result
    /// is stale and only the lease is let go). After a full body, `validators` replace the saved
    /// ones exactly (a response without them clears them); `None` keeps the saved ones.
    async fn finish(&self, db: &PgPool, status: &str, error: Option<&str>, counts: Option<Value>, validators: Option<(&str, Option<&str>, Option<&str>)>) -> sqlx::Result<()> {
        let done = sqlx::query(
            "UPDATE catalog_feeds SET last_status = $4, last_error = $5, last_counts = COALESCE($6, last_counts),
                etag = CASE WHEN $7 THEN $8 ELSE etag END, last_modified = CASE WHEN $7 THEN $9 ELSE last_modified END,
                validator_url = CASE WHEN $7 THEN $10 ELSE validator_url END,
                last_synced_at = now(), next_sync_at = now() + make_interval(hours => interval_hours), updated_at = now(),
                sync_lease = NULL, sync_lease_until = NULL
             WHERE id = $1 AND sync_lease = $2 AND config_version = $3",
        )
        .bind(self.feed_id)
        .bind(self.token)
        .bind(self.version)
        .bind(status)
        .bind(error.map(|e| e.chars().take(1000).collect::<String>()))
        .bind(counts)
        .bind(validators.is_some())
        .bind(validators.and_then(|v| v.1))
        .bind(validators.and_then(|v| v.2))
        .bind(validators.map(|v| v.0))
        .execute(db)
        .await?
        .rows_affected();
        if done == 0 {
            self.release(db).await?;
        }
        Ok(())
    }
}

/// Parsing a big feed is CPU work: it runs off the async workers.
async fn parse(format: String, bytes: Vec<u8>, map: Value, url: String) -> Result<(Vec<ProductInput>, Vec<String>), String> {
    tokio::task::spawn_blocking(move || if format == "google_xml" { parse_google_xml(&bytes, &map) } else { parse_json(&bytes, &map, &url) })
        .await
        .unwrap_or_else(|e| Err(format!("couldn't read the feed ({e})")))
}

/// Job: fetch a feed and save its products. Problems are recorded on the feed (shown in the
/// dashboard), not retried, so a broken feed isn't fetched over and over. While another sync of the
/// same feed runs, this one waits its turn.
pub async fn run_sync(state: &AppState, payload: &Value) -> anyhow::Result<()> {
    let j: SyncJob = serde_json::from_value(payload.clone())?;
    let db = &state.db;
    let token = Uuid::now_v7();
    let claimed: Option<Claimed> = sqlx::query_as(
        "UPDATE catalog_feeds SET sync_lease = $2, sync_lease_until = now() + make_interval(mins => $3), sync_queued_at = NULL
         WHERE id = $1 AND enabled AND (sync_lease IS NULL OR sync_lease_until < now())
         RETURNING account_id, url, format, field_map, etag, last_modified, validator_url, unpublish_missing, config_version, sync_full",
    )
    .bind(j.feed_id)
    .bind(token)
    .bind(LEASE_MINUTES)
    .fetch_optional(db)
    .await?;
    let Some(f) = claimed else {
        let busy: Option<bool> = sqlx::query_scalar("SELECT enabled AND sync_lease_until > now() FROM catalog_feeds WHERE id = $1").bind(j.feed_id).fetch_optional(db).await?.flatten();
        if busy == Some(true) {
            // Another sync of this feed is running: run after it.
            crate::jobs::enqueue(db, "catalog_feed_sync", payload.clone(), Some(Utc::now() + chrono::Duration::seconds(30))).await?;
        } else {
            sqlx::query("UPDATE catalog_feeds SET sync_queued_at = NULL WHERE id = $1").bind(j.feed_id).execute(db).await?;
        }
        return Ok(());
    };
    let lease = Lease { feed_id: j.feed_id, token, version: f.config_version };
    let started = Utc::now();
    let saved = Validators { url: f.validator_url.as_deref(), etag: f.etag.as_deref(), last_modified: f.last_modified.as_deref() };
    let fetched = fetch(&f.url, (!f.sync_full).then_some(saved), MAX_FEED_BYTES, None).await;
    let (bytes, final_url, new_etag, new_lm) = match fetched {
        Ok(Fetched::NotModified) => return Ok(lease.finish(db, "not_modified", None, None, None).await?),
        Ok(Fetched::Body { bytes, url, etag, last_modified }) => (bytes, url, etag, last_modified),
        Err(e) => return Ok(lease.finish(db, "error", Some(&e.public_message()), None, None).await?),
    };
    let (inputs, mut errors) = match parse(f.format.clone(), bytes, f.field_map.clone(), final_url.clone()).await {
        Ok(p) => p,
        Err(e) => return Ok(lease.finish(db, "error", Some(&e), None, None).await?),
    };
    if inputs.is_empty() {
        let msg = errors.first().cloned().unwrap_or_else(|| "the feed has no products".into());
        return Ok(lease.finish(db, "error", Some(&msg), Some(json!({"fetched": 0, "errors": errors.len()})), None).await?);
    }
    let parse_errors = errors.len() as u64;
    let mut total = store::UpsertSummary::default();
    for chunk in inputs.chunks(SYNC_CHUNK) {
        // The feed's address, format or mapping changed (or it was turned off) since this sync
        // started: what it read is out of date. Stop; the sync queued by that change does the rest.
        if !lease.current(db).await? {
            lease.release(db).await?;
            return Ok(());
        }
        let s = store::upsert_products(db, f.account_id, chunk, Origin::Feed(j.feed_id)).await.map_err(|e| anyhow::anyhow!(e.public_message()))?;
        total.created += s.created;
        total.updated += s.updated;
        total.unchanged += s.unchanged;
        total.failed += s.failed;
        errors.extend(s.errors);
    }
    // Only a complete, clean read says what left the feed: with items skipped (unreadable, invalid
    // or past the limit), a missing product may just be one of them.
    let complete = errors.is_empty();
    let removed = if f.unpublish_missing && complete && lease.current(db).await? {
        sqlx::query(
            "UPDATE catalog_products SET published = false, missing_since = now(), updated_at = now()
             WHERE feed_id = $1 AND missing_since IS NULL AND (feed_seen_at IS NULL OR feed_seen_at < $2)",
        )
        .bind(j.feed_id)
        .bind(started)
        .execute(db)
        .await?
        .rows_affected()
    } else {
        0
    };
    let counts = json!({"fetched": inputs.len(), "created": total.created, "updated": total.updated, "unchanged": total.unchanged,
                        "failed": total.failed + parse_errors, "unpublished": removed});
    let note = (!errors.is_empty()).then(|| {
        let shown: Vec<&str> = errors.iter().take(5).map(String::as_str).collect();
        let kept = if f.unpublish_missing { " Products missing from this read were left as they were." } else { "" };
        format!("{} item(s) skipped: {}.{kept}", errors.len(), shown.join("; "))
    });
    lease.finish(db, "ok", note.as_deref(), Some(counts), Some((&final_url, new_etag.as_deref(), new_lm.as_deref()))).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn google_rss_with_variants() {
        let xml = r#"<?xml version="1.0"?>
<rss version="2.0" xmlns:g="http://base.google.com/ns/1.0"><channel><title>Shop</title>
<item><g:id>TS-S</g:id><g:title>Tee</g:title><g:link>https://shop.test/tee</g:link><g:image_link>https://shop.test/tee.jpg</g:image_link>
<g:price>20.00 USD</g:price><g:sale_price>15.00 USD</g:sale_price><g:availability>in_stock</g:availability><g:brand>Acme</g:brand>
<g:product_type>Apparel &gt; Shirts</g:product_type><g:item_group_id>TS</g:item_group_id><g:size>S</g:size><g:custom_label_0>summer</g:custom_label_0></item>
<item><g:id>TS-M</g:id><g:title>Tee</g:title><g:link>https://shop.test/tee</g:link><g:price>20.00 USD</g:price><g:availability>out of stock</g:availability>
<g:item_group_id>TS</g:item_group_id><g:size>M</g:size></item>
<item><g:id>MUG</g:id><title>Mug</title><link>https://shop.test/mug</link><g:price>9.50 EUR</g:price><g:availability>out_of_stock</g:availability></item>
</channel></rss>"#;
        let (items, errors) = parse_google_xml(xml.as_bytes(), &json!({})).unwrap();
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(items.len(), 2);
        let mug = items.iter().find(|p| p.external_id == "MUG").unwrap();
        assert_eq!((mug.price, mug.currency.as_deref(), mug.available), (Some(9.5), Some("EUR"), Some(false)));
        let tee = items.iter().find(|p| p.external_id == "TS").unwrap();
        let vs = tee.variants.as_ref().unwrap();
        assert_eq!(vs.len(), 2);
        assert_eq!((vs[0].price, vs[0].compare_at_price, vs[0].title.as_deref()), (Some(15.0), Some(20.0), Some("S")));
        assert_eq!(vs[1].available, Some(false));
        assert_eq!(tee.categories.as_deref(), Some(&["Apparel".to_string(), "Shirts".to_string()][..]));
        assert_eq!(tee.tags.as_deref(), Some(&["summer".to_string()][..]));
    }

    #[test]
    fn atom_and_field_map() {
        let xml = r#"<feed xmlns="http://www.w3.org/2005/Atom" xmlns:g="http://base.google.com/ns/1.0">
<entry><g:id>1</g:id><title>Cap</title><link href="https://shop.test/cap"/><g:price>12 USD</g:price><g:custom_label_2>Hats</g:custom_label_2><g:color>Red</g:color></entry></feed>"#;
        let (items, _) = parse_google_xml(xml.as_bytes(), &json!({"categories": "g:custom_label_2", "custom.colour": "g:color"})).unwrap();
        assert_eq!(items[0].url.as_deref(), Some("https://shop.test/cap"));
        assert_eq!(items[0].categories.as_deref(), Some(&["Hats".to_string()][..]));
        assert_eq!(items[0].custom.as_ref().unwrap()["colour"], "Red");
    }

    #[test]
    fn xml_size_is_bounded_and_groups_keep_their_items() {
        let item = |id: &str, g: &str| format!("<item><g:id>{id}</g:id><g:title>T</g:title><g:price>1 USD</g:price><g:item_group_id>{g}</g:item_group_id></item>");
        // Groups interleaved: each one still gets exactly its own items, in feed order.
        let items: String = (0..3000).map(|i| item(&format!("v{i}"), &format!("g{}", i % 1000))).collect();
        let xml = format!(r#"<rss xmlns:g="http://base.google.com/ns/1.0"><channel>{items}</channel></rss>"#);
        let (out, errors) = parse_google_xml(xml.as_bytes(), &json!({})).unwrap();
        assert!(errors.is_empty());
        assert_eq!(out.len(), 1000);
        let g7 = out.iter().find(|p| p.external_id == "g7").unwrap();
        let ids: Vec<&str> = g7.variants.as_ref().unwrap().iter().map(|v| v.external_id.as_str()).collect();
        assert_eq!(ids, vec!["v7", "v1007", "v2007"]);
        let err = parse_xml(xml.as_bytes(), &json!({}), 500).unwrap_err();
        assert!(err.contains("more than 500 XML nodes"), "{err}");
    }

    #[test]
    fn feed_addresses_with_credentials_are_refused() {
        assert!(check_url("https://user:secret@shop.test/feed.xml").is_err());
        assert!(check_url("https://token@shop.test/feed.xml").is_err());
        assert!(check_url("https://shop.test/feed.xml?key=abc").is_ok());
    }

    #[test]
    fn xml_with_a_dtd_is_refused() {
        let xml = r#"<?xml version="1.0"?><!DOCTYPE rss [<!ENTITY a "aaaaaaaaaa">]><rss><channel></channel></rss>"#;
        assert!(parse_google_xml(xml.as_bytes(), &json!({})).is_err());
    }

    #[test]
    fn shopify_products_json() {
        let body = json!({"products": [{"id": 77, "title": "Boot", "handle": "boot", "body_html": "<p>Warm</p>", "vendor": "Acme", "product_type": "Shoes",
            "tags": "winter, sale", "images": [{"src": "https://cdn.test/boot.jpg"}],
            "variants": [{"id": 1, "title": "Default Title", "price": "80.00", "compare_at_price": "100.00", "available": true, "sku": "B-1"}]}]});
        let (items, errors) = parse_json(body.to_string().as_bytes(), &json!({}), "https://shop.test/products.json").unwrap();
        assert!(errors.is_empty());
        let p = &items[0];
        assert_eq!(p.external_id, "77");
        assert_eq!(p.url.as_deref(), Some("https://shop.test/products/boot"));
        assert_eq!(p.image_url.as_deref(), Some("https://cdn.test/boot.jpg"));
        assert_eq!(p.tags.as_ref().unwrap().len(), 2);
        let v = &p.variants.as_ref().unwrap()[0];
        assert_eq!((v.price, v.compare_at_price, v.title.clone()), (Some(80.0), Some(100.0), None));
    }

    #[test]
    fn csv_rows_become_products_and_variants() {
        let csv = "id,title,price,variant_id,variant title,inventory,colour\nTEE,Tee,20,TEE-S,Small,0,red\nTEE,,22,TEE-M,Medium,3,\nMUG,Mug,9,,,,blue\n";
        let (items, errors) = parse_csv(csv).unwrap();
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(items.len(), 2);
        let tee = &items[0];
        assert_eq!(tee.title.as_deref(), Some("Tee"));
        let vs = tee.variants.as_ref().unwrap();
        assert_eq!((vs[0].inventory, vs[1].price, vs[1].title.as_deref()), (Some(0), Some(22.0), Some("Medium")));
        assert_eq!(tee.custom.as_ref().unwrap()["colour"], "red");
        assert!(parse_csv("title,price\nA,1\n").is_err());
    }

    #[test]
    fn json_items_path_and_map() {
        let body = json!({"data": {"catalog": [{"code": "X1", "label": "Lamp", "cost": "30", "stock_level": 0}]}});
        let map = json!({"items_path": "data.catalog", "external_id": "code", "title": "label", "price": "cost", "inventory": "stock_level"});
        let (items, _) = parse_json(body.to_string().as_bytes(), &map, "https://x.test/feed.json").unwrap();
        assert_eq!((items[0].external_id.as_str(), items[0].title.as_deref(), items[0].price, items[0].inventory), ("X1", Some("Lamp"), Some(30.0), Some(0)));
        assert!(check_map(&json!({"nope": "x"})).is_err());
        assert!(check_map(&json!({"custom.size": "s"})).is_ok());
    }
}
