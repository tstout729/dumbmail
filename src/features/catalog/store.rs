//! Products and variants: validation, upserts that track price and stock changes, reads.
//!
//! [`upsert_products`] is the one way authoritative product data gets in (REST, MCP, CSV, feeds,
//! and later the Shopify app). It is safe to call in bulk: each product is its own short
//! transaction, unchanged products are skipped by content hash, and restocks and price drops are
//! handed to background jobs queued in the same transaction as the change, so none is lost.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{json, Map, Value};
use sqlx::{PgPool, Postgres, QueryBuilder};
use uuid::Uuid;

use crate::error::{AppError, AppResult};

/// Most products one call may upsert.
pub const MAX_BATCH: usize = 1000;
const MAX_VARIANTS: usize = 250;
const MAX_IMAGES: usize = 20;
const MAX_LABELS: usize = 50;
const MAX_CUSTOM_BYTES: usize = 16 * 1024;

fn de_opt_string<'de, D: Deserializer<'de>>(d: D) -> Result<Option<String>, D::Error> {
    Ok(match Option::<Value>::deserialize(d)? {
        Some(Value::String(s)) => Some(s),
        Some(Value::Number(n)) => Some(n.to_string()),
        Some(Value::Bool(b)) => Some(b.to_string()),
        _ => None,
    })
}

fn de_string<'de, D: Deserializer<'de>>(d: D) -> Result<String, D::Error> {
    de_opt_string(d).map(|s| s.unwrap_or_default())
}

/// A price from a number or text such as "19.99", "19.99 USD" or "$1,299.00".
pub fn parse_price(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => {
            let cleaned: String = s.chars().filter(|c| c.is_ascii_digit() || *c == '.' || *c == ',' || *c == '-').collect();
            let cleaned = match (cleaned.rfind('.'), cleaned.rfind(',')) {
                (Some(d), Some(c)) if c > d => cleaned.replace('.', "").replace(',', "."),
                (None, Some(c)) if cleaned.len() - c == 3 => cleaned.replace(',', "."),
                _ => cleaned.replace(',', ""),
            };
            cleaned.parse().ok()
        }
        _ => None,
    }
    .filter(|p: &f64| p.is_finite())
}

fn de_opt_price<'de, D: Deserializer<'de>>(d: D) -> Result<Option<f64>, D::Error> {
    Ok(Option::<Value>::deserialize(d)?.as_ref().and_then(parse_price))
}

fn de_opt_int<'de, D: Deserializer<'de>>(d: D) -> Result<Option<i64>, D::Error> {
    Ok(Option::<Value>::deserialize(d)?.as_ref().and_then(parse_price).map(|f| f.round() as i64))
}

/// true/false, "yes"/"no", 1/0, or a Google availability word ("in_stock", "out of stock").
pub fn parse_bool(v: &Value) -> Option<bool> {
    match v {
        Value::Bool(b) => Some(*b),
        Value::Number(n) => n.as_f64().map(|f| f != 0.0),
        Value::String(s) => {
            let k: String = s.to_lowercase().chars().filter(|c| c.is_alphanumeric()).collect();
            match k.as_str() {
                "true" | "yes" | "y" | "1" | "instock" | "available" | "preorder" | "backorder" | "published" | "active" | "visible" => Some(true),
                "false" | "no" | "n" | "0" | "outofstock" | "soldout" | "unavailable" | "discontinued" | "draft" | "hidden" | "archived" => Some(false),
                _ => None,
            }
        }
        _ => None,
    }
}

fn de_opt_bool<'de, D: Deserializer<'de>>(d: D) -> Result<Option<bool>, D::Error> {
    Ok(Option::<Value>::deserialize(d)?.as_ref().and_then(parse_bool))
}

/// A list from an array or from text separated by commas, `|` or `>` (category paths).
pub fn parse_list(v: &Value) -> Vec<String> {
    match v {
        Value::Array(a) => a.iter().filter_map(|x| match x {
            Value::String(s) => Some(s.trim().to_string()),
            Value::Number(n) => Some(n.to_string()),
            Value::Object(o) => o.get("title").or_else(|| o.get("name")).and_then(|t| t.as_str()).map(|s| s.trim().to_string()),
            _ => None,
        }).filter(|s| !s.is_empty()).collect(),
        Value::String(s) => s.split([',', '|', '>']).map(|x| x.trim().to_string()).filter(|x| !x.is_empty()).collect(),
        _ => Vec::new(),
    }
}

fn de_opt_list<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Vec<String>>, D::Error> {
    Ok(Option::<Value>::deserialize(d)?.filter(|v| !v.is_null()).map(|v| parse_list(&v)))
}

/// A product as the API, MCP, CSV and feeds send it. Fields left out keep their saved value.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct ProductInput {
    #[serde(alias = "id", deserialize_with = "de_string")]
    pub external_id: String,
    #[serde(default, alias = "name", deserialize_with = "de_opt_string")]
    pub title: Option<String>,
    #[serde(default, deserialize_with = "de_opt_string")]
    pub description: Option<String>,
    #[serde(default, deserialize_with = "de_opt_string")]
    pub url: Option<String>,
    #[serde(default, deserialize_with = "de_opt_string")]
    pub image_url: Option<String>,
    #[serde(default, deserialize_with = "de_opt_list")]
    pub images: Option<Vec<String>>,
    #[serde(default, deserialize_with = "de_opt_price")]
    pub price: Option<f64>,
    /// The original price when on sale. 0 clears it.
    #[serde(default, deserialize_with = "de_opt_price")]
    pub compare_at_price: Option<f64>,
    #[serde(default, deserialize_with = "de_opt_string")]
    pub currency: Option<String>,
    #[serde(default, deserialize_with = "de_opt_bool")]
    pub available: Option<bool>,
    #[serde(default, deserialize_with = "de_opt_int")]
    pub inventory: Option<i64>,
    #[serde(default, deserialize_with = "de_opt_list")]
    pub categories: Option<Vec<String>>,
    /// Merged into `categories`: DumbMail treats collections and categories the same.
    #[serde(default, deserialize_with = "de_opt_list", skip_serializing)]
    pub collections: Option<Vec<String>>,
    #[serde(default, deserialize_with = "de_opt_list")]
    pub tags: Option<Vec<String>>,
    #[serde(default, deserialize_with = "de_opt_string")]
    pub brand: Option<String>,
    #[serde(default, deserialize_with = "de_opt_string")]
    pub vendor: Option<String>,
    #[serde(default)]
    pub custom: Option<Map<String, Value>>,
    #[serde(default, deserialize_with = "de_opt_bool")]
    pub published: Option<bool>,
    #[serde(default)]
    pub published_at: Option<DateTime<Utc>>,
    /// The full list of variants. Left out: the saved variants stay as they are.
    #[serde(default)]
    pub variants: Option<Vec<VariantInput>>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct VariantInput {
    #[serde(alias = "id", deserialize_with = "de_string")]
    pub external_id: String,
    #[serde(default, deserialize_with = "de_opt_string")]
    pub title: Option<String>,
    #[serde(default)]
    pub options: Option<Map<String, Value>>,
    #[serde(default, deserialize_with = "de_opt_string")]
    pub sku: Option<String>,
    #[serde(default, deserialize_with = "de_opt_price")]
    pub price: Option<f64>,
    #[serde(default, deserialize_with = "de_opt_price")]
    pub compare_at_price: Option<f64>,
    #[serde(default, deserialize_with = "de_opt_bool")]
    pub available: Option<bool>,
    #[serde(default, alias = "inventory_quantity", deserialize_with = "de_opt_int")]
    pub inventory: Option<i64>,
    #[serde(default, alias = "image", deserialize_with = "de_opt_string")]
    pub image_url: Option<String>,
}

/// Where an upsert came from. Feeds tag their products so a sync can unpublish the ones that left the feed.
#[derive(Debug, Clone, Copy)]
pub enum Origin {
    Api,
    Csv,
    Feed(Uuid),
    /// The connected Shopify store's Admin API (see `features::shopify`).
    Shopify,
}

impl Origin {
    fn name(&self) -> &'static str {
        match self {
            Origin::Api => "api",
            Origin::Csv => "csv",
            Origin::Feed(_) => "feed",
            Origin::Shopify => "shopify",
        }
    }
    fn feed_id(&self) -> Option<Uuid> {
        match self {
            Origin::Feed(id) => Some(*id),
            _ => None,
        }
    }
}

#[derive(Debug, Default, Serialize)]
pub struct UpsertSummary {
    pub created: u64,
    pub updated: u64,
    pub unchanged: u64,
    pub failed: u64,
    /// The first 50 problems, each naming the product.
    pub errors: Vec<String>,
    /// Saved products' ids, in input order (failed ones left out).
    pub ids: Vec<Uuid>,
}

impl UpsertSummary {
    pub fn error(&mut self, msg: String) {
        self.failed += 1;
        if self.errors.len() < 50 {
            self.errors.push(msg);
        }
    }
}

fn clip(s: Option<&str>, max: usize, what: &str) -> AppResult<Option<String>> {
    match s.map(str::trim).filter(|s| !s.is_empty()) {
        None => Ok(None),
        Some(s) if s.chars().count() > max => Err(AppError::bad(format!("{what} is longer than {max} characters"))),
        Some(s) => Ok(Some(s.to_string())),
    }
}

/// Only http(s) links are kept; anything else (javascript:, data:) is refused.
pub fn clean_url(s: Option<&str>, what: &str) -> AppResult<Option<String>> {
    let Some(s) = clip(s, 2000, what)? else { return Ok(None) };
    let s = if s.starts_with("//") { format!("https:{s}") } else { s };
    match url::Url::parse(&s) {
        Ok(u) if matches!(u.scheme(), "http" | "https") && u.host_str().is_some() => Ok(Some(u.to_string())),
        _ => Err(AppError::bad(format!("{what} must be an http(s) link"))),
    }
}

fn clean_price(p: Option<f64>, what: &str) -> AppResult<Option<f64>> {
    match p {
        None => Ok(None),
        Some(p) if !(0.0..=1e9).contains(&p) => Err(AppError::bad(format!("{what} must be between 0 and 1,000,000,000"))),
        Some(p) => Ok(Some((p * 10000.0).round() / 10000.0)),
    }
}

fn clean_labels(v: Option<&Vec<String>>, what: &str) -> AppResult<Option<Vec<String>>> {
    let Some(v) = v else { return Ok(None) };
    let mut out: Vec<String> = Vec::new();
    for s in v {
        let s = s.trim();
        if s.is_empty() || out.iter().any(|x| x.eq_ignore_ascii_case(s)) {
            continue;
        }
        if s.chars().count() > 200 {
            return Err(AppError::bad(format!("each of {what} must be at most 200 characters")));
        }
        out.push(s.to_string());
    }
    if out.len() > MAX_LABELS {
        return Err(AppError::bad(format!("at most {MAX_LABELS} {what}")));
    }
    Ok(Some(out))
}

fn clean_inventory(v: Option<i64>) -> Option<i32> {
    v.map(|i| i.clamp(-1_000_000_000, 1_000_000_000) as i32)
}

/// Validated, trimmed product input.
#[derive(Debug, Clone, Serialize)]
pub struct Clean {
    pub external_id: String,
    pub title: Option<String>,
    pub description: Option<String>,
    pub url: Option<String>,
    pub image_url: Option<String>,
    pub images: Option<Vec<String>>,
    pub price: Option<f64>,
    pub compare_at_price: Option<f64>,
    pub currency: Option<String>,
    pub available: Option<bool>,
    pub inventory: Option<i32>,
    pub categories: Option<Vec<String>>,
    pub tags: Option<Vec<String>>,
    pub brand: Option<String>,
    pub vendor: Option<String>,
    pub custom: Option<Value>,
    pub published: Option<bool>,
    pub published_at: Option<DateTime<Utc>>,
    pub variants: Option<Vec<CleanVariant>>,
}

#[derive(Debug, Clone, Serialize)]
pub struct CleanVariant {
    pub external_id: String,
    pub title: Option<String>,
    pub options: Value,
    pub sku: Option<String>,
    pub price: Option<f64>,
    pub compare_at_price: Option<f64>,
    pub available: bool,
    pub inventory: Option<i32>,
    pub image_url: Option<String>,
}

impl CleanVariant {
    pub fn in_stock(&self) -> bool {
        self.available && self.inventory.is_none_or(|i| i > 0)
    }
}

pub fn clean(p: &ProductInput) -> AppResult<Clean> {
    let external_id = p.external_id.trim().to_string();
    if external_id.is_empty() || external_id.chars().count() > 255 {
        return Err(AppError::bad("each product needs an id (external_id) of 1-255 characters"));
    }
    let ctx = |e: AppError| AppError::bad(format!("product {external_id:?}: {}", e.public_message()));
    let currency = match p.currency.as_deref().map(str::trim).filter(|c| !c.is_empty()) {
        None => None,
        Some(c) if c.len() == 3 && c.chars().all(|x| x.is_ascii_alphabetic()) => Some(c.to_ascii_uppercase()),
        Some(_) => return Err(ctx(AppError::bad("currency must be a 3-letter code such as USD"))),
    };
    let images = match &p.images {
        None => None,
        Some(list) => {
            let mut out = Vec::new();
            for u in list.iter().take(MAX_IMAGES) {
                if let Some(u) = clean_url(Some(u), "image").map_err(ctx)? {
                    out.push(u);
                }
            }
            Some(out)
        }
    };
    let custom = match &p.custom {
        None => None,
        Some(m) => {
            let v = Value::Object(m.clone());
            if v.to_string().len() > MAX_CUSTOM_BYTES {
                return Err(ctx(AppError::bad("custom fields are larger than 16 KB")));
            }
            Some(v)
        }
    };
    let mut categories = p.categories.clone();
    if let Some(c) = &p.collections {
        categories.get_or_insert_with(Vec::new).extend(c.iter().cloned());
    }
    let variants = match &p.variants {
        None => None,
        Some(vs) if vs.len() > MAX_VARIANTS => return Err(ctx(AppError::bad(format!("at most {MAX_VARIANTS} variants")))),
        Some(vs) => {
            let mut out: Vec<CleanVariant> = Vec::with_capacity(vs.len());
            for v in vs {
                let id = v.external_id.trim().to_string();
                if id.is_empty() || id.chars().count() > 255 {
                    return Err(ctx(AppError::bad("each variant needs an id of 1-255 characters")));
                }
                if out.iter().any(|x| x.external_id == id) {
                    return Err(ctx(AppError::bad(format!("variant id {id:?} appears twice"))));
                }
                let options = Value::Object(v.options.clone().unwrap_or_default());
                if options.to_string().len() > 4096 {
                    return Err(ctx(AppError::bad("variant options are larger than 4 KB")));
                }
                out.push(CleanVariant {
                    external_id: id,
                    title: clip(v.title.as_deref(), 300, "variant title").map_err(ctx)?,
                    options,
                    sku: clip(v.sku.as_deref(), 255, "sku").map_err(ctx)?,
                    price: clean_price(v.price, "variant price").map_err(ctx)?,
                    compare_at_price: clean_price(v.compare_at_price, "variant compare-at price").map_err(ctx)?,
                    available: v.available.unwrap_or(true),
                    inventory: clean_inventory(v.inventory),
                    image_url: clean_url(v.image_url.as_deref(), "variant image").map_err(ctx)?,
                });
            }
            Some(out)
        }
    };
    Ok(Clean {
        title: clip(p.title.as_deref(), 500, "title").map_err(ctx)?,
        description: clip(p.description.as_deref(), 20_000, "description").map_err(ctx)?,
        url: clean_url(p.url.as_deref(), "url").map_err(ctx)?,
        image_url: clean_url(p.image_url.as_deref(), "image_url").map_err(ctx)?,
        images,
        price: clean_price(p.price, "price").map_err(ctx)?,
        compare_at_price: clean_price(p.compare_at_price, "compare-at price").map_err(ctx)?,
        currency,
        available: p.available,
        inventory: clean_inventory(p.inventory),
        categories: clean_labels(categories.as_ref(), "categories").map_err(ctx)?,
        tags: clean_labels(p.tags.as_ref(), "tags").map_err(ctx)?,
        brand: clip(p.brand.as_deref(), 200, "brand").map_err(ctx)?,
        vendor: clip(p.vendor.as_deref(), 200, "vendor").map_err(ctx)?,
        custom,
        published: p.published,
        published_at: p.published_at,
        variants,
        external_id,
    })
}

/// A restock or price drop found while saving a product, handled by a background job.
#[derive(Debug, Clone, PartialEq)]
pub enum Change {
    /// The product (variant None) or one variant went from out of stock to in stock.
    Restock { product_id: Uuid, variant_id: Option<Uuid> },
    PriceDrop { product_id: Uuid, old: f64, new: f64 },
}

enum Saved {
    Created(Uuid),
    Updated(Uuid),
    Unchanged(Uuid),
}

/// Save products from an authoritative source (not events). Problems with one product are
/// reported in the summary and don't stop the rest. Products saved here count as verified.
pub async fn upsert_products(db: &PgPool, account_id: Uuid, inputs: &[ProductInput], origin: Origin) -> AppResult<UpsertSummary> {
    upsert_products_with(db, account_id, inputs, origin, false).await
}

/// [`upsert_products`], optionally `quiet`: prices and stock are saved (and their history) but no
/// back-in-stock or price-drop alerts are queued. For a first load or a history import, where a
/// "change" is only the catalog catching up, not something that happened in the store.
pub async fn upsert_products_with(db: &PgPool, account_id: Uuid, inputs: &[ProductInput], origin: Origin, quiet: bool) -> AppResult<UpsertSummary> {
    if inputs.len() > MAX_BATCH && !matches!(origin, Origin::Feed(_) | Origin::Csv | Origin::Shopify) {
        return Err(AppError::bad(format!("send at most {MAX_BATCH} products per call")));
    }
    let mut out = UpsertSummary::default();
    for p in inputs {
        let c = match clean(p) {
            Ok(c) => c,
            Err(e) => {
                out.error(e.public_message());
                continue;
            }
        };
        match upsert_one(db, account_id, &c, origin, quiet).await {
            Ok(Saved::Created(id)) => {
                out.created += 1;
                out.ids.push(id);
            }
            Ok(Saved::Updated(id)) => {
                out.updated += 1;
                out.ids.push(id);
            }
            Ok(Saved::Unchanged(id)) => {
                out.unchanged += 1;
                out.ids.push(id);
            }
            Err(AppError::Internal(e)) => return Err(AppError::Internal(e)),
            Err(e) => out.error(e.public_message()),
        }
    }
    Ok(out)
}

#[derive(sqlx::FromRow)]
struct Existing {
    id: Uuid,
    content_hash: Option<String>,
    source: String,
    feed_id: Option<Uuid>,
    price: Option<f64>,
    in_stock: bool,
    missing_since: Option<DateTime<Utc>>,
}

async fn upsert_one(db: &PgPool, account_id: Uuid, c: &Clean, origin: Origin, quiet: bool) -> AppResult<Saved> {
    let hash = crate::auth::sha256_hex(&serde_json::to_string(c).map_err(|e| AppError::Internal(e.into()))?);
    let mut tx = db.begin().await?;
    let existing: Option<Existing> = sqlx::query_as(
        "SELECT id, content_hash, source, feed_id, price, in_stock, missing_since FROM catalog_products WHERE account_id = $1 AND external_id = $2 FOR UPDATE",
    )
    .bind(account_id)
    .bind(&c.external_id)
    .fetch_optional(&mut *tx)
    .await?;
    if let Some(e) = &existing {
        if e.content_hash.as_deref() == Some(hash.as_str()) && e.source == origin.name() && e.feed_id == origin.feed_id() {
            if origin.feed_id().is_some() {
                // Back in the feed after leaving it: it shows again if the store publishes it.
                sqlx::query(
                    "UPDATE catalog_products SET feed_seen_at = now(), missing_since = NULL, published = source_published,
                        updated_at = CASE WHEN missing_since IS NULL THEN updated_at ELSE now() END WHERE id = $1",
                )
                .bind(e.id)
                .execute(&mut *tx)
                .await?;
                if e.missing_since.is_some() {
                    tx.commit().await?;
                    return Ok(Saved::Updated(e.id));
                }
            }
            tx.commit().await?;
            return Ok(Saved::Unchanged(e.id));
        }
    }
    let old_variants: Vec<(Uuid, String, bool)> = match &existing {
        Some(e) => sqlx::query_as("SELECT id, external_id, in_stock FROM catalog_variants WHERE product_id = $1").bind(e.id).fetch_all(&mut *tx).await?,
        None => Vec::new(),
    };
    // The product's price: its own, else its cheapest variant's.
    let variant_min = c.variants.as_ref().and_then(|vs| vs.iter().filter_map(|v| v.price).reduce(f64::min));
    let price = c.price.or(variant_min);
    let compare_at = c.compare_at_price.or_else(|| {
        c.variants.as_ref().and_then(|vs| vs.iter().filter(|v| v.price == variant_min).find_map(|v| v.compare_at_price))
    });
    let id = match &existing {
        None => {
            let title = c.title.clone().ok_or_else(|| AppError::bad(format!("product {:?}: a new product needs a title", c.external_id)))?;
            let id = Uuid::now_v7();
            sqlx::query(
                "INSERT INTO catalog_products (id, account_id, external_id, title, description, url, image_url, images, price, compare_at_price,
                    currency, available, inventory, categories, tags, brand, vendor, custom, published, source_published, published_at, source, feed_id, feed_seen_at, content_hash, verified)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, COALESCE($8, '[]'::jsonb), $9, NULLIF($10, 0), $11, COALESCE($12, true), $13, COALESCE($14, '{}'), COALESCE($15, '{}'),
                    $16, $17, COALESCE($18, '{}'::jsonb), COALESCE($19, true), COALESCE($19, true), COALESCE($20, now()), $21, $22, CASE WHEN $22::uuid IS NULL THEN NULL ELSE now() END, $23, true)",
            )
            .bind(id)
            .bind(account_id)
            .bind(&c.external_id)
            .bind(&title)
            .bind(&c.description)
            .bind(&c.url)
            .bind(c.image_url.clone().or_else(|| c.images.as_ref().and_then(|i| i.first().cloned())))
            .bind(c.images.as_ref().map(|i| json!(i)))
            .bind(price)
            .bind(compare_at)
            .bind(&c.currency)
            .bind(c.available)
            .bind(c.inventory)
            .bind(&c.categories)
            .bind(&c.tags)
            .bind(&c.brand)
            .bind(&c.vendor)
            .bind(&c.custom)
            .bind(c.published)
            .bind(c.published_at)
            .bind(origin.name())
            .bind(origin.feed_id())
            .bind(&hash)
            .execute(&mut *tx)
            .await?;
            id
        }
        Some(e) => {
            sqlx::query(
                "UPDATE catalog_products SET title = COALESCE($2, title), description = COALESCE($3, description), url = COALESCE($4, url),
                    image_url = COALESCE($5, image_url), images = COALESCE($6, images), price = COALESCE($7, price),
                    compare_at_price = CASE WHEN $8::float8 IS NULL THEN compare_at_price WHEN $8 = 0 THEN NULL ELSE $8 END,
                    currency = COALESCE($9, currency), available = COALESCE($10, available),
                    inventory = COALESCE($11, inventory),
                    categories = COALESCE($12, categories), tags = COALESCE($13, tags), brand = COALESCE($14, brand), vendor = COALESCE($15, vendor),
                    custom = COALESCE($16, custom), source_published = COALESCE($17, source_published),
                    published = COALESCE($17, source_published) AND ($20::uuid IS NOT NULL OR missing_since IS NULL),
                    missing_since = CASE WHEN $20::uuid IS NULL THEN missing_since ELSE NULL END, published_at = COALESCE($18, published_at),
                    source = $19, feed_id = COALESCE($20, feed_id), feed_seen_at = CASE WHEN $20::uuid IS NULL THEN feed_seen_at ELSE now() END,
                    content_hash = $21, verified = true, verified_at = COALESCE(verified_at, now()), verify_after = NULL, verify_error = NULL, updated_at = now()
                 WHERE id = $1",
            )
            .bind(e.id)
            .bind(&c.title)
            .bind(&c.description)
            .bind(&c.url)
            .bind(c.image_url.clone().or_else(|| c.images.as_ref().and_then(|i| i.first().cloned())))
            .bind(c.images.as_ref().map(|i| json!(i)))
            .bind(price)
            .bind(compare_at)
            .bind(&c.currency)
            .bind(c.available)
            .bind(c.inventory)
            .bind(&c.categories)
            .bind(&c.tags)
            .bind(&c.brand)
            .bind(&c.vendor)
            .bind(&c.custom)
            .bind(c.published)
            .bind(c.published_at)
            .bind(origin.name())
            .bind(origin.feed_id())
            .bind(&hash)
            .execute(&mut *tx)
            .await?;
            e.id
        }
    };

    let mut variant_ids: Vec<(Uuid, bool, bool)> = Vec::new(); // (id, was in stock (None: new), now in stock)
    if let Some(vs) = &c.variants {
        let keep: Vec<&str> = vs.iter().map(|v| v.external_id.as_str()).collect();
        sqlx::query("DELETE FROM catalog_variants WHERE product_id = $1 AND NOT (external_id = ANY($2))")
            .bind(id)
            .bind(&keep)
            .execute(&mut *tx)
            .await?;
        for (pos, v) in vs.iter().enumerate() {
            let vid: Uuid = sqlx::query_scalar(
                "INSERT INTO catalog_variants (id, account_id, product_id, external_id, title, options, sku, price, compare_at_price, available, inventory, in_stock, image_url, position)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14)
                 ON CONFLICT (product_id, external_id) DO UPDATE SET title = EXCLUDED.title, options = EXCLUDED.options, sku = EXCLUDED.sku,
                    price = EXCLUDED.price, compare_at_price = EXCLUDED.compare_at_price, available = EXCLUDED.available, inventory = EXCLUDED.inventory,
                    in_stock = EXCLUDED.in_stock, image_url = EXCLUDED.image_url, position = EXCLUDED.position, updated_at = now()
                 RETURNING id",
            )
            .bind(Uuid::now_v7())
            .bind(account_id)
            .bind(id)
            .bind(&v.external_id)
            .bind(&v.title)
            .bind(&v.options)
            .bind(&v.sku)
            .bind(v.price)
            .bind(v.compare_at_price)
            .bind(v.available)
            .bind(v.inventory)
            .bind(v.in_stock())
            .bind(&v.image_url)
            .bind(pos as i32)
            .fetch_one(&mut *tx)
            .await?;
            let was = old_variants.iter().find(|o| o.1 == v.external_id).map(|o| o.2);
            if let Some(was) = was {
                variant_ids.push((vid, was, v.in_stock()));
            }
            if was != Some(v.in_stock()) {
                sqlx::query("INSERT INTO catalog_price_history (account_id, product_id, variant_id, price, compare_at_price, in_stock, inventory) VALUES ($1, $2, $3, $4, $5, $6, $7)")
                    .bind(account_id)
                    .bind(id)
                    .bind(vid)
                    .bind(v.price)
                    .bind(v.compare_at_price)
                    .bind(v.in_stock())
                    .bind(v.inventory)
                    .execute(&mut *tx)
                    .await?;
            }
        }
    }
    // In stock: any variant in stock; a product without variants is in stock when available with inventory left (or untracked).
    let (new_price, new_compare, new_in_stock, inventory): (Option<f64>, Option<f64>, bool, Option<i32>) = sqlx::query_as(
        "UPDATE catalog_products p SET in_stock = CASE WHEN EXISTS (SELECT 1 FROM catalog_variants v WHERE v.product_id = p.id)
                THEN EXISTS (SELECT 1 FROM catalog_variants v WHERE v.product_id = p.id AND v.in_stock)
                ELSE p.available AND COALESCE(p.inventory, 1) > 0 END
         WHERE p.id = $1 RETURNING price, compare_at_price, in_stock, inventory",
    )
    .bind(id)
    .fetch_one(&mut *tx)
    .await?;
    let price_changed = existing.as_ref().is_none_or(|e| e.price != new_price);
    let stock_changed = existing.as_ref().is_none_or(|e| e.in_stock != new_in_stock);
    if price_changed || stock_changed {
        sqlx::query("INSERT INTO catalog_price_history (account_id, product_id, price, compare_at_price, in_stock, inventory) VALUES ($1, $2, $3, $4, $5, $6)")
            .bind(account_id)
            .bind(id)
            .bind(new_price)
            .bind(new_compare)
            .bind(new_in_stock)
            .bind(inventory)
            .execute(&mut *tx)
            .await?;
    }
    // Restocks and price drops are queued in this transaction: they're saved exactly when the change is.
    if let Some(e) = existing.as_ref().filter(|_| !quiet) {
        let mut changes = Vec::new();
        for (vid, was, now) in &variant_ids {
            if !*was && *now {
                changes.push(Change::Restock { product_id: id, variant_id: Some(*vid) });
            }
        }
        if !e.in_stock && new_in_stock && variant_ids.is_empty() {
            changes.push(Change::Restock { product_id: id, variant_id: None });
        }
        if let (Some(old), Some(new)) = (e.price, new_price) {
            if new < old {
                changes.push(Change::PriceDrop { product_id: id, old, new });
            }
        }
        for ch in &changes {
            super::alerts::enqueue_change(&mut tx, account_id, ch).await?;
        }
    }
    tx.commit().await?;
    Ok(if existing.is_some() { Saved::Updated(id) } else { Saved::Created(id) })
}

/// A product row as the API returns it.
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct Product {
    pub id: Uuid,
    pub external_id: String,
    pub title: String,
    pub description: Option<String>,
    pub url: Option<String>,
    pub image_url: Option<String>,
    pub images: Value,
    pub price: Option<f64>,
    pub compare_at_price: Option<f64>,
    pub currency: Option<String>,
    pub available: bool,
    pub inventory: Option<i32>,
    pub in_stock: bool,
    pub categories: Vec<String>,
    pub tags: Vec<String>,
    pub brand: Option<String>,
    pub vendor: Option<String>,
    pub custom: Value,
    pub published: bool,
    pub published_at: DateTime<Utc>,
    pub source: String,
    pub feed_id: Option<Uuid>,
    /// False for a product learned from browser events that the server hasn't confirmed yet: it
    /// shows only to the person whose own event carried it.
    pub verified: bool,
    /// Why verifying it from its page hasn't worked (yet).
    pub verify_error: Option<String>,
    /// When it left its feed (it's unpublished until it comes back).
    pub missing_since: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

pub const PRODUCT_COLS: &str = "cp.id, cp.external_id, cp.title, cp.description, cp.url, cp.image_url, cp.images, cp.price, cp.compare_at_price, cp.currency,
    cp.available, cp.inventory, cp.in_stock, cp.categories, cp.tags, cp.brand, cp.vendor, cp.custom, cp.published, cp.published_at, cp.source, cp.feed_id,
    cp.verified, cp.verify_error, cp.missing_since, cp.created_at, cp.updated_at";

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct Variant {
    pub id: Uuid,
    pub external_id: String,
    pub title: Option<String>,
    pub options: Value,
    pub sku: Option<String>,
    pub price: Option<f64>,
    pub compare_at_price: Option<f64>,
    pub available: bool,
    pub inventory: Option<i32>,
    pub in_stock: bool,
    pub image_url: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
pub struct ListQuery {
    #[serde(default)]
    pub q: Option<String>,
    #[serde(default)]
    pub category: Option<String>,
    #[serde(default)]
    pub tag: Option<String>,
    #[serde(default)]
    pub brand: Option<String>,
    #[serde(default)]
    pub in_stock: Option<bool>,
    #[serde(default)]
    pub published: Option<bool>,
    #[serde(default)]
    pub on_sale: Option<bool>,
    #[serde(default)]
    pub verified: Option<bool>,
    #[serde(default)]
    pub limit: Option<i64>,
    #[serde(default)]
    pub offset: Option<i64>,
}

fn push_filters(qb: &mut QueryBuilder<'_, Postgres>, account_id: Uuid, q: &ListQuery) {
    qb.push(" FROM catalog_products cp WHERE cp.account_id = ").push_bind(account_id);
    if let Some(s) = q.q.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        let s: String = s.chars().take(200).collect();
        let like = format!("%{}%", s.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_"));
        qb.push(" AND (cp.title ILIKE ").push_bind(like.clone()).push(" OR cp.external_id = ").push_bind(s.clone());
        qb.push(" OR cp.brand ILIKE ").push_bind(like);
        qb.push(" OR EXISTS (SELECT 1 FROM catalog_variants v WHERE v.product_id = cp.id AND (v.sku = ").push_bind(s.clone()).push(" OR v.external_id = ").push_bind(s).push(")))");
    }
    if let Some(c) = q.category.as_deref().filter(|s| !s.is_empty()) {
        qb.push(" AND cp.categories @> ARRAY[").push_bind(c.to_string()).push("]::text[]");
    }
    if let Some(t) = q.tag.as_deref().filter(|s| !s.is_empty()) {
        qb.push(" AND cp.tags @> ARRAY[").push_bind(t.to_string()).push("]::text[]");
    }
    if let Some(b) = q.brand.as_deref().filter(|s| !s.is_empty()) {
        qb.push(" AND lower(cp.brand) = lower(").push_bind(b.to_string()).push(")");
    }
    if let Some(s) = q.in_stock {
        qb.push(" AND cp.in_stock = ").push_bind(s);
    }
    if let Some(p) = q.published {
        qb.push(" AND cp.published = ").push_bind(p);
    }
    if q.on_sale == Some(true) {
        qb.push(" AND cp.compare_at_price > cp.price");
    }
    if let Some(v) = q.verified {
        qb.push(" AND cp.verified = ").push_bind(v);
    }
}

pub async fn list(db: &PgPool, account_id: Uuid, q: &ListQuery) -> AppResult<(Vec<Product>, i64)> {
    let mut qb = QueryBuilder::new("SELECT count(*)");
    push_filters(&mut qb, account_id, q);
    let total: i64 = qb.build_query_scalar().fetch_one(db).await?;
    let mut qb = QueryBuilder::new(format!("SELECT {PRODUCT_COLS}"));
    push_filters(&mut qb, account_id, q);
    qb.push(" ORDER BY cp.updated_at DESC, cp.id LIMIT ").push_bind(q.limit.unwrap_or(50).clamp(1, 200));
    qb.push(" OFFSET ").push_bind(q.offset.unwrap_or(0).max(0));
    Ok((qb.build_query_as::<Product>().fetch_all(db).await?, total))
}

/// A product by DumbMail id or by the store's own id.
pub async fn find(db: &PgPool, account_id: Uuid, key: &str) -> AppResult<Product> {
    let q = format!("SELECT {PRODUCT_COLS} FROM catalog_products cp WHERE cp.account_id = $1 AND (cp.id::text = $2 OR cp.external_id = $2) ORDER BY (cp.id::text = $2) DESC LIMIT 1");
    sqlx::query_as::<_, Product>(&q).bind(account_id).bind(key.trim()).fetch_optional(db).await?.ok_or_else(|| AppError::not_found("product"))
}

pub async fn variants(db: &PgPool, product_id: Uuid) -> AppResult<Vec<Variant>> {
    Ok(sqlx::query_as(
        "SELECT id, external_id, title, options, sku, price, compare_at_price, available, inventory, in_stock, image_url FROM catalog_variants WHERE product_id = $1 ORDER BY position, external_id",
    )
    .bind(product_id)
    .fetch_all(db)
    .await?)
}

/// A product with its variants, price and stock history, and who's waiting for a restock.
pub async fn detail(db: &PgPool, account_id: Uuid, key: &str) -> AppResult<Value> {
    let p = find(db, account_id, key).await?;
    let vs = variants(db, p.id).await?;
    let history: Vec<(Option<Uuid>, Option<f64>, Option<f64>, bool, Option<i32>, DateTime<Utc>)> = sqlx::query_as(
        "SELECT variant_id, price, compare_at_price, in_stock, inventory, recorded_at FROM catalog_price_history WHERE product_id = $1 ORDER BY recorded_at DESC, id DESC LIMIT 200",
    )
    .bind(p.id)
    .fetch_all(db)
    .await?;
    let waiting: Vec<(Uuid, Option<String>, Option<Uuid>, DateTime<Utc>, bool)> = sqlx::query_as(
        "SELECT s.profile_id, pr.email, s.variant_id, s.created_at, s.confirmed_at IS NOT NULL FROM catalog_stock_subscriptions s JOIN profiles pr ON pr.id = s.profile_id
         WHERE s.product_id = $1 AND s.notified_at IS NULL ORDER BY s.created_at LIMIT 100",
    )
    .bind(p.id)
    .fetch_all(db)
    .await?;
    let (waiting_total, unconfirmed_total): (i64, i64) = sqlx::query_as(
        "SELECT count(*) FILTER (WHERE confirmed_at IS NOT NULL), count(*) FILTER (WHERE confirmed_at IS NULL)
         FROM catalog_stock_subscriptions WHERE product_id = $1 AND notified_at IS NULL",
    )
    .bind(p.id)
    .fetch_one(db)
    .await?;
    let counts: Vec<(String, i64)> = sqlx::query_as(
        "SELECT kind, count(DISTINCT event_id) FROM catalog_interactions WHERE product_id = $1 AND occurred_at > now() - interval '30 days' GROUP BY kind",
    )
    .bind(p.id)
    .fetch_all(db)
    .await?;
    let mut v = serde_json::to_value(&p).map_err(|e| AppError::Internal(e.into()))?;
    v["variants"] = json!(vs);
    v["history"] = json!(history.iter().map(|h| json!({"variant_id": h.0, "price": h.1, "compare_at_price": h.2, "in_stock": h.3, "inventory": h.4, "recorded_at": h.5})).collect::<Vec<_>>());
    v["waiting"] = json!(waiting.iter().map(|w| json!({"profile_id": w.0, "email": w.1, "variant_id": w.2, "since": w.3, "confirmed": w.4})).collect::<Vec<_>>());
    v["waiting_total"] = json!(waiting_total);
    v["unconfirmed_total"] = json!(unconfirmed_total);
    v["activity_30d"] = Value::Object(counts.into_iter().map(|(k, n)| (k, json!(n))).collect());
    Ok(v)
}

pub async fn delete(db: &PgPool, account_id: Uuid, key: &str) -> AppResult<Product> {
    let p = find(db, account_id, key).await?;
    sqlx::query("DELETE FROM catalog_products WHERE id = $1 AND account_id = $2").bind(p.id).bind(account_id).execute(db).await?;
    Ok(p)
}

/// Counts for the Products page header.
pub async fn summary(db: &PgPool, account_id: Uuid) -> AppResult<Value> {
    let (total, in_stock, published, from_events, unverified): (i64, i64, i64, i64, i64) = sqlx::query_as(
        "SELECT count(*), count(*) FILTER (WHERE in_stock), count(*) FILTER (WHERE published), count(*) FILTER (WHERE source = 'events'), count(*) FILTER (WHERE NOT verified)
         FROM catalog_products WHERE account_id = $1",
    )
    .bind(account_id)
    .fetch_one(db)
    .await?;
    let waiting: i64 = sqlx::query_scalar("SELECT count(*) FROM catalog_stock_subscriptions WHERE account_id = $1 AND notified_at IS NULL AND confirmed_at IS NOT NULL")
        .bind(account_id)
        .fetch_one(db)
        .await?;
    Ok(json!({"products": total, "in_stock": in_stock, "published": published, "learned_from_events": from_events, "unverified": unverified, "waiting_for_restock": waiting}))
}

/// Distinct categories, tags and brands (most used first), for pickers.
pub async fn facets(db: &PgPool, account_id: Uuid) -> AppResult<Value> {
    let categories: Vec<String> = sqlx::query_scalar(
        "SELECT c FROM catalog_products, unnest(categories) c WHERE account_id = $1 GROUP BY c ORDER BY count(*) DESC, c LIMIT 300",
    )
    .bind(account_id)
    .fetch_all(db)
    .await?;
    let tags: Vec<String> = sqlx::query_scalar("SELECT t FROM catalog_products, unnest(tags) t WHERE account_id = $1 GROUP BY t ORDER BY count(*) DESC, t LIMIT 300")
        .bind(account_id)
        .fetch_all(db)
        .await?;
    let brands: Vec<String> = sqlx::query_scalar(
        "SELECT brand FROM catalog_products WHERE account_id = $1 AND brand IS NOT NULL GROUP BY brand ORDER BY count(*) DESC, brand LIMIT 300",
    )
    .bind(account_id)
    .fetch_all(db)
    .await?;
    Ok(json!({"categories": categories, "tags": tags, "brands": brands}))
}

/// "$1,299.00", "€12.50", "¥1200" or "12.00 CHF".
pub fn money(v: f64, currency: Option<&str>) -> String {
    let cur = currency.unwrap_or("USD");
    let (sym, decimals, suffix) = match cur {
        "USD" => ("$", 2, false),
        "CAD" => ("CA$", 2, false),
        "AUD" => ("A$", 2, false),
        "NZD" => ("NZ$", 2, false),
        "EUR" => ("€", 2, false),
        "GBP" => ("£", 2, false),
        "JPY" => ("¥", 0, false),
        "INR" => ("₹", 2, false),
        _ => ("", 2, true),
    };
    let rounded = format!("{:.*}", decimals, v);
    let (int, frac) = rounded.split_once('.').map(|(a, b)| (a.to_string(), Some(b.to_string()))).unwrap_or((rounded.clone(), None));
    let neg = int.starts_with('-');
    let digits: Vec<char> = int.trim_start_matches('-').chars().collect();
    let mut grouped = String::new();
    for (i, ch) in digits.iter().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            grouped.push(',');
        }
        grouped.push(*ch);
    }
    let num = format!("{}{}{}", if neg { "-" } else { "" }, grouped, frac.map(|f| format!(".{f}")).unwrap_or_default());
    if suffix { format!("{num} {cur}") } else { format!("{sym}{num}") }
}

/// A product as emails and templates see it.
pub fn card(p: &Product) -> Value {
    let on_sale = matches!((p.price, p.compare_at_price), (Some(a), Some(b)) if b > a);
    let short: String = p.description.as_deref().map(|d| {
        static TAGS: once_cell::sync::Lazy<regex::Regex> = once_cell::sync::Lazy::new(|| regex::Regex::new(r"<[^>]+>").expect("static regex"));
        TAGS.replace_all(d, " ").split_whitespace().collect::<Vec<_>>().join(" ").chars().take(300).collect()
    }).unwrap_or_default();
    json!({
        "id": p.external_id,
        "title": p.title,
        "name": p.title,
        "url": p.url,
        "image_url": p.image_url,
        "price": p.price,
        "compare_at_price": p.compare_at_price,
        "currency": p.currency,
        "price_text": p.price.map(|v| money(v, p.currency.as_deref())),
        "compare_at_text": p.compare_at_price.filter(|_| on_sale).map(|v| money(v, p.currency.as_deref())),
        "on_sale": on_sale,
        "in_stock": p.in_stock,
        "categories": p.categories,
        "tags": p.tags,
        "brand": p.brand,
        "description": short,
    })
}

/// A product card for one variant: the variant's price, stock and image where it has them.
pub fn variant_card(p: &Product, v: &Variant) -> Value {
    let mut c = card(p);
    let price = v.price.or(p.price);
    let compare = if v.price.is_some() { v.compare_at_price } else { p.compare_at_price };
    let on_sale = matches!((price, compare), (Some(a), Some(b)) if b > a);
    c["variant_id"] = json!(v.external_id);
    c["variant"] = json!(v.title);
    c["price"] = json!(price);
    c["compare_at_price"] = json!(compare);
    c["price_text"] = json!(price.map(|x| money(x, p.currency.as_deref())));
    c["compare_at_text"] = json!(compare.filter(|_| on_sale).map(|x| money(x, p.currency.as_deref())));
    c["on_sale"] = json!(on_sale);
    c["in_stock"] = json!(v.in_stock);
    if let Some(img) = &v.image_url {
        c["image_url"] = json!(img);
    }
    c
}

pub async fn variants_by_ids(db: &PgPool, ids: &[Uuid]) -> AppResult<Vec<(Uuid, Variant)>> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let rows: Vec<VariantRow> = sqlx::query_as(
        "SELECT product_id, id, external_id, title, options, sku, price, compare_at_price, available, inventory, in_stock, image_url FROM catalog_variants WHERE id = ANY($1)",
    )
    .bind(ids)
    .fetch_all(db)
    .await?;
    Ok(rows.into_iter().map(|r| (r.product_id, r.variant)).collect())
}

#[derive(sqlx::FromRow)]
struct VariantRow {
    product_id: Uuid,
    #[sqlx(flatten)]
    variant: Variant,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prices_from_text() {
        assert_eq!(parse_price(&json!("19.99 USD")), Some(19.99));
        assert_eq!(parse_price(&json!("$1,299.00")), Some(1299.0));
        assert_eq!(parse_price(&json!("1.299,50 €")), Some(1299.5));
        assert_eq!(parse_price(&json!("12,50")), Some(12.5));
        assert_eq!(parse_price(&json!(7)), Some(7.0));
        assert_eq!(parse_price(&json!("free")), None);
    }

    #[test]
    fn money_formats() {
        assert_eq!(money(1299.0, Some("USD")), "$1,299.00");
        assert_eq!(money(12.5, Some("EUR")), "€12.50");
        assert_eq!(money(1200.0, Some("JPY")), "¥1,200");
        assert_eq!(money(9.0, Some("CHF")), "9.00 CHF");
        assert_eq!(money(999999.0, None), "$999,999.00");
    }

    #[test]
    fn clean_rejects_bad_input() {
        let ok: ProductInput = serde_json::from_value(json!({"id": 123, "title": "Mug", "price": "12.00", "available": "in stock", "tags": "a, b, a", "collections": ["Kitchen"]})).unwrap();
        let c = clean(&ok).unwrap();
        assert_eq!(c.external_id, "123");
        assert_eq!(c.price, Some(12.0));
        assert_eq!(c.available, Some(true));
        assert_eq!(c.tags, Some(vec!["a".into(), "b".into()]));
        assert_eq!(c.categories, Some(vec!["Kitchen".into()]));
        for bad in [
            json!({"id": "", "title": "x"}),
            json!({"id": "a", "url": "javascript:alert(1)"}),
            json!({"id": "a", "currency": "dollars"}),
            json!({"id": "a", "price": -1}),
            json!({"id": "a", "variants": [{"id": "v"}, {"id": "v"}]}),
        ] {
            let p: ProductInput = serde_json::from_value(bad.clone()).unwrap();
            assert!(clean(&p).is_err(), "{bad}");
        }
    }
}
