//! Reading product activity from events.
//!
//! The website snippet and the Shopify pixel already send Viewed Product, Added to Cart, Started
//! Checkout and Placed Order with product details. A background job reads new events in id order
//! (event ids are time-ordered), records who did what with which product in
//! `catalog_interactions`, and, for stores without a feed, adds the products it sees so the
//! catalog fills itself. Products from a feed, the API or a CSV are never changed by events.
//!
//! Anyone can send events from a browser, so a learned product starts unverified: it shows only to
//! the person whose own event carried it, until `verify` reads its page on the store's site. Orders
//! from the browser are recorded but don't count as sales (only trusted ones do).

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use serde_json::{json, Value};
use sqlx::PgPool;
use uuid::Uuid;

use super::store::parse_price;
use crate::state::AppState;

/// Events read per job run (of every metric, so the scan stays bounded).
const SCAN: i64 = 2000;
/// Items read from one event.
const MAX_ITEMS: usize = 50;

pub fn kind_for(metric: &str) -> Option<&'static str> {
    match metric {
        "Viewed Product" => Some("viewed"),
        "Added to Cart" => Some("carted"),
        "Started Checkout" => Some("checkout"),
        "Placed Order" => Some("ordered"),
        _ => None,
    }
}

/// The products an event is about: the viewed or added product itself, or the cart/order items.
pub fn event_items(metric: &str, props: &Value) -> Vec<Value> {
    let single = || if props.get("name").is_some() || props.get("product_id").is_some() { vec![props.clone()] } else { Vec::new() };
    let items = || props.get("items").and_then(|i| i.as_array()).map(|a| a.iter().filter(|x| x.is_object()).take(MAX_ITEMS).cloned().collect()).unwrap_or_default();
    match metric {
        "Viewed Product" | "Added to Cart" => single(),
        "Started Checkout" | "Placed Order" => items(),
        // Our own alert events and custom events: their items, else the product they name.
        _ => {
            let i: Vec<Value> = items();
            if i.is_empty() { single() } else { i }
        }
    }
}

pub fn text(v: &Value, k: &str) -> Option<String> {
    match v.get(k)? {
        Value::String(s) => Some(s.trim().to_string()).filter(|s| !s.is_empty()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
    .map(|s| s.chars().take(500).collect())
}

fn http_url(v: &Value, k: &str) -> Option<String> {
    text(v, k).filter(|u| u.starts_with("https://") || u.starts_with("http://")).filter(|u| u.len() <= 2000)
}

/// A page URL without its query string or fragment, with and without the trailing slash.
fn url_keys(u: &str) -> Vec<String> {
    let base = u.split(['?', '#']).next().unwrap_or("").trim_end_matches('/').to_string();
    if base.is_empty() { Vec::new() } else { vec![base.clone(), format!("{base}/")] }
}

/// Find the catalog product (and variant) an event item names: product id, then variant id, then SKU, then page URL.
pub async fn resolve(db: &PgPool, account_id: Uuid, item: &Value) -> sqlx::Result<Option<(Uuid, Option<Uuid>, String)>> {
    let pid = text(item, "product_id");
    let vid = text(item, "variant_id");
    let sku = text(item, "sku");
    let mut found: Option<(Uuid, String)> = None;
    for key in [&pid, &sku].into_iter().flatten() {
        if found.is_none() {
            found = sqlx::query_as("SELECT id, source FROM catalog_products WHERE account_id = $1 AND external_id = $2").bind(account_id).bind(key).fetch_optional(db).await?;
        }
    }
    let mut variant: Option<Uuid> = None;
    if let Some(v) = &vid {
        let hit: Option<(Uuid, Uuid)> = match &found {
            Some((p, _)) => sqlx::query_as("SELECT id, product_id FROM catalog_variants WHERE product_id = $1 AND external_id = $2").bind(p).bind(v).fetch_optional(db).await?,
            None => sqlx::query_as("SELECT id, product_id FROM catalog_variants WHERE account_id = $1 AND external_id = $2 LIMIT 1").bind(account_id).bind(v).fetch_optional(db).await?,
        };
        if let Some((vid, prod)) = hit {
            variant = Some(vid);
            if found.is_none() {
                found = sqlx::query_as("SELECT id, source FROM catalog_products WHERE id = $1").bind(prod).fetch_optional(db).await?;
            }
        }
    }
    if found.is_none() {
        if let Some(s) = &sku {
            let hit: Option<(Uuid, Uuid)> = sqlx::query_as("SELECT id, product_id FROM catalog_variants WHERE account_id = $1 AND sku = $2 LIMIT 1").bind(account_id).bind(s).fetch_optional(db).await?;
            if let Some((v, prod)) = hit {
                variant = variant.or(Some(v));
                found = sqlx::query_as("SELECT id, source FROM catalog_products WHERE id = $1").bind(prod).fetch_optional(db).await?;
            }
        }
    }
    if found.is_none() {
        if let Some(u) = http_url(item, "url") {
            let keys = url_keys(&u);
            if !keys.is_empty() {
                found = sqlx::query_as("SELECT id, source FROM catalog_products WHERE account_id = $1 AND url = ANY($2) ORDER BY updated_at DESC LIMIT 1")
                    .bind(account_id)
                    .bind(&keys)
                    .fetch_optional(db)
                    .await?;
            }
        }
    }
    Ok(found.map(|(p, src)| (p, variant, src)))
}

/// Add (or refresh) a product seen only in events. Returns its id and variant id.
async fn learn_product(db: &PgPool, account_id: Uuid, item: &Value, existing: Option<Uuid>) -> sqlx::Result<Option<(Uuid, Option<Uuid>)>> {
    let Some(name) = text(item, "name").or_else(|| text(item, "title")) else { return Ok(existing.map(|p| (p, None))) };
    let url = http_url(item, "url");
    let external_id = match text(item, "product_id").or_else(|| text(item, "sku")).or_else(|| url.as_deref().and_then(|u| url_keys(u).into_iter().next())) {
        Some(id) => id.chars().take(255).collect::<String>(),
        None => return Ok(existing.map(|p| (p, None))),
    };
    let price = item.get("price").and_then(parse_price).filter(|p| (0.0..=1e9).contains(p));
    let currency = text(item, "currency").filter(|c| c.len() == 3 && c.chars().all(|x| x.is_ascii_alphabetic())).map(|c| c.to_ascii_uppercase());
    let categories: Option<Vec<String>> = item.get("categories").map(super::store::parse_list).map(|v| v.into_iter().take(20).map(|c| c.chars().take(200).collect()).collect());
    let image = http_url(item, "image_url");
    let brand = text(item, "brand").map(|b| b.chars().take(200).collect::<String>());
    // Until it's verified, the newest event's details stand in. Once the store's page has confirmed
    // it, the page wins: events only ask for a fresh look now and then.
    let fresh = "catalog_products.source = 'events' AND NOT catalog_products.verified";
    let q = format!(
        "INSERT INTO catalog_products (id, account_id, external_id, title, url, image_url, price, currency, categories, brand, source, verified, verify_after)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, COALESCE($9, '{{}}'), $10, 'events', false, now())
         ON CONFLICT (account_id, external_id) DO UPDATE SET
            title = CASE WHEN {fresh} THEN EXCLUDED.title ELSE catalog_products.title END,
            url = CASE WHEN {fresh} THEN COALESCE(EXCLUDED.url, catalog_products.url) ELSE catalog_products.url END,
            image_url = CASE WHEN {fresh} THEN COALESCE(EXCLUDED.image_url, catalog_products.image_url) ELSE catalog_products.image_url END,
            price = CASE WHEN {fresh} THEN COALESCE(EXCLUDED.price, catalog_products.price) ELSE catalog_products.price END,
            currency = CASE WHEN {fresh} THEN COALESCE(EXCLUDED.currency, catalog_products.currency) ELSE catalog_products.currency END,
            categories = CASE WHEN {fresh} AND $9::text[] IS NOT NULL THEN EXCLUDED.categories ELSE catalog_products.categories END,
            brand = CASE WHEN {fresh} THEN COALESCE(EXCLUDED.brand, catalog_products.brand) ELSE catalog_products.brand END,
            verify_after = CASE WHEN catalog_products.source = 'events' AND catalog_products.verified AND catalog_products.verify_after IS NULL
                                 AND catalog_products.verified_at < now() - interval '1 day' THEN now() ELSE catalog_products.verify_after END,
            updated_at = CASE WHEN {fresh} THEN now() ELSE catalog_products.updated_at END
         RETURNING id"
    );
    let id: Uuid = sqlx::query_scalar(&q)
    .bind(existing.unwrap_or_else(Uuid::now_v7))
    .bind(account_id)
    .bind(&external_id)
    .bind(&name)
    .bind(&url)
    .bind(&image)
    .bind(price)
    .bind(&currency)
    .bind(&categories)
    .bind(&brand)
    .fetch_one(db)
    .await?;
    let mut variant = None;
    if let Some(vid) = text(item, "variant_id") {
        variant = sqlx::query_scalar(
            "INSERT INTO catalog_variants (id, account_id, product_id, external_id, title, sku, price)
             SELECT $1, $2, $3, $4, $5, $6, $7 FROM catalog_products WHERE id = $3 AND source = 'events'
             ON CONFLICT (product_id, external_id) DO UPDATE SET title = COALESCE(EXCLUDED.title, catalog_variants.title),
                sku = COALESCE(EXCLUDED.sku, catalog_variants.sku), price = COALESCE(EXCLUDED.price, catalog_variants.price), updated_at = now()
             RETURNING id",
        )
        .bind(Uuid::now_v7())
        .bind(account_id)
        .bind(id)
        .bind(vid.chars().take(255).collect::<String>())
        .bind(text(item, "variant"))
        .bind(text(item, "sku"))
        .bind(price)
        .fetch_optional(db)
        .await?;
    }
    Ok(Some((id, variant)))
}

/// The store's own hosts: its website and its verified sending domains, without `www.`.
pub fn hosts_of(website: Option<&str>, domains: &[String]) -> Vec<String> {
    let mut out: Vec<String> = domains.iter().map(|d| d.trim().trim_start_matches("www.").to_lowercase()).filter(|d| !d.is_empty()).collect();
    if let Some(w) = website.map(str::trim).filter(|w| !w.is_empty()) {
        let w = if w.contains("://") { w.to_string() } else { format!("https://{w}") };
        if let Some(h) = url::Url::parse(&w).ok().and_then(|u| u.host_str().map(|h| h.trim_start_matches("www.").to_lowercase())) {
            out.push(h);
        }
    }
    out
}

/// Whether a link goes to the store's own site (or a subdomain of it).
pub fn on_site(hosts: &[String], link: &str) -> bool {
    let Some(h) = url::Url::parse(link).ok().and_then(|u| u.host_str().map(|h| h.trim_start_matches("www.").to_lowercase())) else { return false };
    hosts.iter().any(|s| h == *s || h.ends_with(&format!(".{s}")))
}

pub async fn site_hosts(db: &PgPool, account_id: Uuid) -> sqlx::Result<Vec<String>> {
    let website: Option<String> = sqlx::query_scalar("SELECT website FROM accounts WHERE id = $1").bind(account_id).fetch_optional(db).await?.flatten();
    let domains: Vec<String> = sqlx::query_scalar("SELECT domain FROM sending_domains WHERE account_id = $1 AND status = 'verified'").bind(account_id).fetch_all(db).await?;
    Ok(hosts_of(website.as_deref(), &domains))
}

/// What the reader needs to know about an account.
#[derive(Clone)]
pub struct Rules {
    /// New products come from events only while the store has no feed and hasn't turned this off.
    learn: bool,
    /// Anyone can send events from a browser, so a product is only learned (or refreshed) when its
    /// page is on the store's own site: a made-up product can't link people elsewhere.
    hosts: Vec<String>,
}

async fn rules_for(db: &PgPool, account_id: Uuid) -> sqlx::Result<Rules> {
    let learn: bool = sqlx::query_scalar(
        "SELECT COALESCE((SELECT learn_from_events FROM catalog_settings WHERE account_id = $1), true)
            AND NOT EXISTS (SELECT 1 FROM catalog_feeds WHERE account_id = $1 AND enabled)",
    )
    .bind(account_id)
    .fetch_one(db)
    .await?;
    Ok(Rules { learn, hosts: site_hosts(db, account_id).await? })
}

/// One event to read: id, account, profile, metric, properties, when, and whether a browser sent it
/// (`None`: recorded before that was tracked).
pub type EventRow<'a> = (Uuid, Uuid, Uuid, &'a str, &'a Value, DateTime<Utc>, Option<bool>);

/// Record one event's products. Returns how many items it recorded.
pub async fn record(db: &PgPool, rules_cache: &mut HashMap<Uuid, Rules>, ev: EventRow<'_>) -> sqlx::Result<usize> {
    let (event_id, account_id, profile_id, metric, props, at, from_browser) = ev;
    // Only what the server, the API or an integration recorded counts as a sale.
    let trusted = from_browser == Some(false);
    let Some(kind) = kind_for(metric) else { return Ok(0) };
    let mut n = 0;
    for item in event_items(metric, props) {
        let found = resolve(db, account_id, &item).await?;
        let rules = match rules_cache.get(&account_id) {
            Some(r) => r.clone(),
            None => {
                let r = rules_for(db, account_id).await?;
                rules_cache.insert(account_id, r.clone());
                r
            }
        };
        let trusted_link = http_url(&item, "url").is_some_and(|u| on_site(&rules.hosts, &u));
        let ids = match found {
            Some((p, v, src)) if src == "events" && trusted_link => learn_product(db, account_id, &item, Some(p)).await?.map(|(p, v2)| (p, v.or(v2))),
            Some((p, v, _)) => Some((p, v)),
            None if rules.learn && trusted_link => learn_product(db, account_id, &item, None).await?,
            None => None,
        };
        let Some((product_id, variant_id)) = ids else { continue };
        let qty = item.get("quantity").and_then(parse_price).map(|q| q.round().clamp(1.0, 10_000.0) as i32).unwrap_or(1);
        sqlx::query(
            "INSERT INTO catalog_interactions (account_id, profile_id, product_id, variant_id, kind, quantity, price, event_id, occurred_at, trusted)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10) ON CONFLICT (event_id, product_id, kind) DO NOTHING",
        )
        .bind(account_id)
        .bind(profile_id)
        .bind(product_id)
        .bind(variant_id)
        .bind(kind)
        .bind(qty)
        .bind(item.get("price").and_then(parse_price))
        .bind(event_id)
        .bind(at)
        .bind(trusted)
        .execute(db)
        .await?;
        n += 1;
    }
    Ok(n)
}

/// Job: read the next batch of events. `lag_secs` leaves the newest events for the next run, so an
/// event whose insert commits a moment late isn't skipped. Returns whether more are waiting.
pub async fn run(state: &AppState, lag_secs: i64) -> anyhow::Result<bool> {
    let mut tx = state.db.begin().await?;
    // One reader at a time; another worker already holding the row means nothing to do here.
    let Some(cursor): Option<Uuid> = sqlx::query_scalar("SELECT last_event FROM catalog_learn_cursor WHERE id = 1 FOR UPDATE SKIP LOCKED").fetch_optional(&mut *tx).await? else {
        return Ok(false);
    };
    let window: Vec<Uuid> = sqlx::query_scalar(
        "SELECT id FROM events WHERE id > $1 AND created_at < now() - make_interval(secs => $2) ORDER BY id LIMIT $3",
    )
    .bind(cursor)
    .bind(lag_secs as f64)
    .bind(SCAN)
    .fetch_all(&mut *tx)
    .await?;
    let Some(last) = window.last().copied() else { return Ok(false) };
    let rows: Vec<(Uuid, Uuid, Uuid, String, Value, DateTime<Utc>, Option<bool>)> = sqlx::query_as(
        "SELECT e.id, e.account_id, e.profile_id, m.name, e.properties, e.occurred_at, e.from_browser FROM events e JOIN metrics m ON m.id = e.metric_id
         WHERE e.id > $1 AND e.id <= $2 AND m.name IN ('Viewed Product', 'Added to Cart', 'Started Checkout', 'Placed Order')
         ORDER BY e.id",
    )
    .bind(cursor)
    .bind(last)
    .fetch_all(&mut *tx)
    .await?;
    let mut cache = HashMap::new();
    for (id, account, profile, metric, props, at, from_browser) in &rows {
        if let Err(e) = record(&state.db, &mut cache, (*id, *account, *profile, metric, props, *at, *from_browser)).await {
            // One bad event must not stall the reader for everyone.
            tracing::warn!(event = %id, error = %e, "couldn't read products from event");
        }
    }
    sqlx::query("UPDATE catalog_learn_cursor SET last_event = $1, updated_at = now() WHERE id = 1").bind(last).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(window.len() as i64 == SCAN)
}

/// Read every event up to now (tests, and catching up after a backlog).
pub async fn catch_up(state: &AppState) -> anyhow::Result<()> {
    for _ in 0..10_000 {
        if !run(state, 0).await? {
            break;
        }
    }
    Ok(())
}

/// A catalog match for an event item: the product and, when the item names one, the variant.
pub type Match = Option<(Uuid, Option<Uuid>)>;

/// [`resolve`] for many items at once, in three queries whatever their number.
pub async fn resolve_many(db: &PgPool, account_id: Uuid, items: &[Value]) -> sqlx::Result<Vec<Match>> {
    let mut keys: Vec<String> = Vec::new();
    let mut vids: Vec<String> = Vec::new();
    let mut skus: Vec<String> = Vec::new();
    let mut urls: Vec<String> = Vec::new();
    for it in items {
        keys.extend(text(it, "product_id"));
        keys.extend(text(it, "sku"));
        vids.extend(text(it, "variant_id"));
        skus.extend(text(it, "sku"));
        if let Some(u) = http_url(it, "url") {
            urls.extend(url_keys(&u));
        }
    }
    if keys.is_empty() && vids.is_empty() && urls.is_empty() {
        return Ok(vec![None; items.len()]);
    }
    let by_ext: Vec<(Uuid, String)> = sqlx::query_as("SELECT id, external_id FROM catalog_products WHERE account_id = $1 AND external_id = ANY($2)")
        .bind(account_id)
        .bind(&keys)
        .fetch_all(db)
        .await?;
    let vars: Vec<(Uuid, Uuid, String, Option<String>)> = sqlx::query_as(
        "SELECT id, product_id, external_id, sku FROM catalog_variants WHERE account_id = $1 AND (external_id = ANY($2) OR sku = ANY($3)) ORDER BY id",
    )
    .bind(account_id)
    .bind(&vids)
    .bind(&skus)
    .fetch_all(db)
    .await?;
    let by_url: Vec<(Uuid, String)> = if urls.is_empty() {
        Vec::new()
    } else {
        sqlx::query_as("SELECT id, url FROM catalog_products WHERE account_id = $1 AND url = ANY($2) ORDER BY updated_at DESC").bind(account_id).bind(&urls).fetch_all(db).await?
    };
    Ok(items
        .iter()
        .map(|it| {
            let (pid, vid, sku) = (text(it, "product_id"), text(it, "variant_id"), text(it, "sku"));
            let ext = |k: &Option<String>| k.as_ref().and_then(|k| by_ext.iter().find(|p| &p.1 == k)).map(|p| p.0);
            let mut product = ext(&pid).or_else(|| ext(&sku));
            let mut variant = None;
            if let Some(v) = &vid {
                let hit = vars.iter().find(|x| &x.2 == v && product.is_none_or(|p| x.1 == p));
                if let Some(h) = hit {
                    variant = Some(h.0);
                    product = product.or(Some(h.1));
                }
            }
            if product.is_none() {
                if let Some(h) = sku.as_ref().and_then(|s| vars.iter().find(|x| x.3.as_deref() == Some(s.as_str()))) {
                    variant = variant.or(Some(h.0));
                    product = Some(h.1);
                }
            }
            if product.is_none() {
                if let Some(u) = http_url(it, "url") {
                    let ks = url_keys(&u);
                    product = by_url.iter().find(|p| ks.contains(&p.1)).map(|p| p.0);
                }
            }
            product.map(|p| (p, variant))
        })
        .collect())
}

/// A product card made only from what one event item says. Its link is kept only when it goes to
/// the store's own site (`hosts`).
pub fn raw_card(item: &Value, props: &Value, hosts: &[String]) -> Value {
    let price = item.get("price").and_then(parse_price);
    let currency = text(item, "currency").or_else(|| text(props, "currency"));
    json!({
        "id": text(item, "product_id"),
        "variant_id": text(item, "variant_id"),
        "title": text(item, "name").or_else(|| text(item, "title")),
        "name": text(item, "name").or_else(|| text(item, "title")),
        "url": http_url(item, "url").filter(|u| on_site(hosts, u)),
        "image_url": http_url(item, "image_url"),
        "price": price,
        "currency": currency,
        "price_text": price.map(|p| super::store::money(p, currency.as_deref())),
        "quantity": item.get("quantity").and_then(parse_price),
        "in_stock": true,
    })
}

/// Items a flow's triggering event carries: each one's catalog match (product and variant) and a
/// card made from what the event itself says.
pub async fn event_cards(db: &PgPool, account_id: Uuid, hosts: &[String], event: &Value) -> sqlx::Result<Vec<(Match, Value)>> {
    let metric = event.get("metric").and_then(|m| m.as_str()).unwrap_or("");
    let props = event.get("properties").cloned().unwrap_or(json!({}));
    let items: Vec<Value> = event_items(metric, &props).into_iter().take(MAX_ITEMS).collect();
    let found = resolve_many(db, account_id, &items).await?;
    Ok(items.iter().zip(found).map(|(it, m)| (m, raw_card(it, &props, hosts))).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn links_must_be_on_the_store_site() {
        let hosts = hosts_of(Some("www.acme.test"), &["mail-brand.test".to_string()]);
        assert!(on_site(&hosts, "https://acme.test/products/x"));
        assert!(on_site(&hosts, "https://shop.acme.test/x"));
        assert!(on_site(&hosts, "https://www.mail-brand.test/x"));
        assert!(!on_site(&hosts, "https://acme.test.evil.example/x"));
        assert!(!on_site(&hosts, "https://evilacme.test/x"));
        assert!(!on_site(&[], "https://acme.test/x"));
    }
}
