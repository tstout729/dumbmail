//! Verifying products learned from browser events.
//!
//! Anyone can send events from a browser, so what an event says about a product (its title,
//! price, image) can't be shown to other people as is. A learned product stays unverified until the
//! server reads its page, on the store's own site only (its website or a verified sending domain),
//! through the same protections as feeds (public addresses only, pinned DNS, each redirect
//! re-checked and kept on the store's site, time limits and a size cap). The page's product data
//! (schema.org JSON-LD, else Open Graph product tags) then replaces what the browser said.
//!
//! The scheduler hands out at most one page a minute per account, as background jobs. A page
//! without product data is tried again later, three times in all. Verified learned products are
//! looked at again at most once a day while their events keep coming, so price changes show up.

use once_cell::sync::Lazy;
use regex::Regex;
use serde_json::{json, Value};
use sqlx::PgPool;
use uuid::Uuid;

use super::learn::{on_site, site_hosts};
use super::store::{parse_bool, parse_price};
use crate::state::AppState;

/// Most bytes of a product page read.
const MAX_PAGE_BYTES: usize = 3 * 1024 * 1024;
/// Tries before giving up on a page without product data.
const MAX_ATTEMPTS: i32 = 3;

/// What a product page says about the product.
#[derive(Debug, Default, PartialEq)]
pub struct PageProduct {
    pub title: Option<String>,
    pub description: Option<String>,
    pub image_url: Option<String>,
    pub price: Option<f64>,
    pub currency: Option<String>,
    pub available: Option<bool>,
    pub brand: Option<String>,
}

fn str_of(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.trim().to_string()).filter(|s| !s.is_empty()),
        Value::Number(n) => Some(n.to_string()),
        Value::Array(a) => a.iter().find_map(str_of),
        Value::Object(o) => o.get("name").or_else(|| o.get("url")).or_else(|| o.get("@id")).and_then(str_of),
        _ => None,
    }
}

fn http(s: Option<String>) -> Option<String> {
    s.filter(|u| (u.starts_with("https://") || u.starts_with("http://")) && u.len() <= 2000)
}

fn is_product(n: &Value) -> bool {
    let t = &n["@type"];
    let check = |s: &str| s == "Product" || s.ends_with("/Product") || s == "ProductGroup";
    match t {
        Value::String(s) => check(s),
        Value::Array(a) => a.iter().any(|x| x.as_str().is_some_and(check)),
        _ => false,
    }
}

fn find_product(v: &Value, depth: usize) -> Option<&Value> {
    if depth > 6 {
        return None;
    }
    match v {
        Value::Array(a) => a.iter().find_map(|x| find_product(x, depth + 1)),
        Value::Object(o) => {
            if is_product(v) {
                return Some(v);
            }
            o.get("@graph").and_then(|g| find_product(g, depth + 1)).or_else(|| o.get("mainEntity").and_then(|g| find_product(g, depth + 1)))
        }
        _ => None,
    }
}

fn from_json_ld(p: &Value) -> PageProduct {
    let offers: Vec<&Value> = match &p["offers"] {
        Value::Array(a) => a.iter().collect(),
        Value::Object(_) => vec![&p["offers"]],
        _ => match &p["hasVariant"] {
            Value::Array(a) => a.iter().map(|v| &v["offers"]).filter(|o| o.is_object()).collect(),
            _ => Vec::new(),
        },
    };
    let price_of = |o: &Value| o.get("price").or_else(|| o.get("lowPrice")).and_then(parse_price);
    let cheapest = offers.iter().filter_map(|o| price_of(o).map(|pr| (pr, *o))).min_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
    let available = if offers.is_empty() {
        None
    } else {
        Some(offers.iter().any(|o| o["availability"].as_str().is_none_or(|a| {
            let a = a.rsplit('/').next().unwrap_or(a).to_ascii_lowercase();
            matches!(a.as_str(), "instock" | "limitedavailability" | "onlineonly" | "preorder" | "presale" | "backorder" | "instoreonly")
        })))
    };
    PageProduct {
        title: str_of(&p["name"]),
        description: str_of(&p["description"]),
        image_url: http(str_of(&p["image"])),
        price: cheapest.map(|c| c.0),
        currency: cheapest.and_then(|c| str_of(&c.1["priceCurrency"])).or_else(|| offers.iter().find_map(|o| str_of(&o["priceCurrency"]))),
        available,
        brand: str_of(&p["brand"]),
    }
}

fn unescape(s: &str) -> String {
    s.replace("&quot;", "\"").replace("&#39;", "'").replace("&#x27;", "'").replace("&lt;", "<").replace("&gt;", ">").replace("&amp;", "&")
}

/// Product data from a page: its schema.org Product JSON-LD, else its Open Graph product tags.
pub fn parse_page(html: &str) -> Option<PageProduct> {
    static LD: Lazy<Regex> = Lazy::new(|| Regex::new(r#"(?is)<script[^>]*type\s*=\s*["']application/ld\+json["'][^>]*>(.*?)</script>"#).expect("static regex"));
    static META: Lazy<Regex> = Lazy::new(|| Regex::new(r#"(?is)<meta\s[^>]*>"#).expect("static regex"));
    static ATTR: Lazy<Regex> = Lazy::new(|| Regex::new(r#"(?is)\b(property|name|content)\s*=\s*("([^"]*)"|'([^']*)')"#).expect("static regex"));
    for cap in LD.captures_iter(html).take(20) {
        let Ok(v) = serde_json::from_str::<Value>(cap[1].trim()) else { continue };
        if let Some(p) = find_product(&v, 0) {
            let out = from_json_ld(p);
            if out.title.is_some() {
                return Some(out);
            }
        }
    }
    let mut meta: Vec<(String, String)> = Vec::new();
    for m in META.find_iter(html).take(500) {
        let (mut key, mut content) = (None, None);
        for a in ATTR.captures_iter(m.as_str()) {
            let val = a.get(3).or_else(|| a.get(4)).map(|x| unescape(x.as_str().trim())).unwrap_or_default();
            if a[1].eq_ignore_ascii_case("content") {
                content = Some(val);
            } else {
                key = Some(val.to_ascii_lowercase());
            }
        }
        if let (Some(k), Some(c)) = (key, content) {
            meta.push((k, c));
        }
    }
    let get = |k: &str| meta.iter().find(|(n, _)| n == k).map(|(_, c)| c.clone()).filter(|c| !c.is_empty());
    let is_product = get("og:type").is_some_and(|t| t.to_ascii_lowercase().contains("product")) || get("product:price:amount").is_some() || get("og:price:amount").is_some();
    if !is_product {
        return None;
    }
    let out = PageProduct {
        title: get("og:title"),
        description: get("og:description"),
        image_url: http(get("og:image:secure_url").or_else(|| get("og:image"))),
        price: get("product:price:amount").or_else(|| get("og:price:amount")).and_then(|p| parse_price(&json!(p))),
        currency: get("product:price:currency").or_else(|| get("og:price:currency")),
        available: get("product:availability").or_else(|| get("og:availability")).and_then(|a| parse_bool(&json!(a))),
        brand: get("product:brand"),
    };
    out.title.is_some().then_some(out)
}

/// Scheduler: hand out due product pages, at most one per account per run, as jobs.
pub async fn queue_due(db: &PgPool) -> anyhow::Result<()> {
    // Claimed for an hour; the job sets the real next time (or clears it).
    let ids: Vec<Uuid> = sqlx::query_scalar(
        "UPDATE catalog_products SET verify_after = now() + interval '1 hour' WHERE id IN (
            SELECT id FROM (SELECT id, row_number() OVER (PARTITION BY account_id ORDER BY verify_after, id) AS rn
                            FROM catalog_products p WHERE verify_after <= now() AND source = 'events'
                              AND NOT EXISTS (SELECT 1 FROM accounts a WHERE a.id = p.account_id AND a.closed_at IS NOT NULL)) x
            WHERE rn = 1 LIMIT 50)
         RETURNING id",
    )
    .fetch_all(db)
    .await?;
    for id in ids {
        crate::jobs::enqueue(db, "catalog_verify", json!({"product_id": id}), None).await?;
    }
    Ok(())
}

async fn give_up_or_retry(db: &PgPool, id: Uuid, attempts: i32, why: &str) -> sqlx::Result<()> {
    let n = attempts + 1;
    sqlx::query(
        "UPDATE catalog_products SET verify_attempts = $2, verify_error = $3,
            verify_after = CASE WHEN verified OR $2 >= $4 THEN NULL ELSE now() + make_interval(hours => 6 * $2) END WHERE id = $1",
    )
    .bind(id)
    .bind(n)
    .bind(why.chars().take(300).collect::<String>())
    .bind(MAX_ATTEMPTS)
    .execute(db)
    .await?;
    Ok(())
}

/// Job: read one learned product's page and, if it has product data, verify the product with it.
pub async fn run(state: &AppState, payload: &Value) -> anyhow::Result<()> {
    let Some(id) = payload["product_id"].as_str().and_then(|s| s.parse::<Uuid>().ok()) else { return Ok(()) };
    let row: Option<(Uuid, Option<String>, String, i32)> = sqlx::query_as("SELECT account_id, url, source, verify_attempts FROM catalog_products WHERE id = $1")
        .bind(id)
        .fetch_optional(&state.db)
        .await?;
    let Some((account_id, url, source, attempts)) = row else { return Ok(()) };
    if source != "events" {
        return Ok(());
    }
    let hosts = site_hosts(&state.db, account_id).await?;
    let Some(url) = url.filter(|u| on_site(&hosts, u)) else {
        give_up_or_retry(&state.db, id, MAX_ATTEMPTS, "the product page isn't on the store's own site").await?;
        return Ok(());
    };
    let page = match super::feeds::fetch(&url, None, MAX_PAGE_BYTES, Some(&hosts)).await {
        Ok(super::feeds::Fetched::Body { bytes, .. }) => String::from_utf8_lossy(&bytes).into_owned(),
        Ok(super::feeds::Fetched::NotModified) => String::new(),
        Err(e) => {
            give_up_or_retry(&state.db, id, attempts, &e.public_message()).await?;
            return Ok(());
        }
    };
    let Some(p) = parse_page(&page) else {
        give_up_or_retry(&state.db, id, attempts, "the page has no product details (schema.org Product or Open Graph product tags)").await?;
        return Ok(());
    };
    let clip = |s: Option<String>, n: usize| s.map(|s| s.chars().take(n).collect::<String>());
    let currency = p.currency.filter(|c| c.len() == 3 && c.chars().all(|x| x.is_ascii_alphabetic())).map(|c| c.to_ascii_uppercase());
    let price = p.price.filter(|x| (0.0..=1e9).contains(x));
    // What the page says wins over what the browser said.
    sqlx::query(
        "UPDATE catalog_products SET title = COALESCE($2, title), description = $3, image_url = $4, price = $5,
            currency = $6, available = COALESCE($7, available), brand = $8,
            in_stock = CASE WHEN EXISTS (SELECT 1 FROM catalog_variants v WHERE v.product_id = catalog_products.id) THEN in_stock ELSE COALESCE($7, available) END,
            verified = true, verified_at = now(), verify_after = NULL, verify_attempts = 0, verify_error = NULL, updated_at = now()
         WHERE id = $1 AND source = 'events'",
    )
    .bind(id)
    .bind(clip(p.title, 500))
    .bind(clip(p.description, 20_000))
    .bind(p.image_url)
    .bind(price)
    .bind(currency)
    .bind(p.available)
    .bind(clip(p.brand, 200))
    .execute(&state.db)
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_json_ld_and_open_graph() {
        let html = r#"<html><head><script type="application/ld+json">{"@context":"https://schema.org","@graph":[{"@type":"WebPage"},
            {"@type":"Product","name":"Wool socks","image":["https://cdn.acme.test/socks.jpg"],"brand":{"@type":"Brand","name":"Acme"},
             "offers":[{"@type":"Offer","price":"14.00","priceCurrency":"USD","availability":"https://schema.org/OutOfStock"},
                       {"@type":"Offer","price":"12.00","priceCurrency":"USD","availability":"https://schema.org/InStock"}]}]}</script></head></html>"#;
        let p = parse_page(html).unwrap();
        assert_eq!((p.title.as_deref(), p.price, p.currency.as_deref(), p.available, p.brand.as_deref()), (Some("Wool socks"), Some(12.0), Some("USD"), Some(true), Some("Acme")));
        assert_eq!(p.image_url.as_deref(), Some("https://cdn.acme.test/socks.jpg"));
        let og = r#"<meta property="og:type" content="product"><meta property="og:title" content="Cap &amp; scarf"><meta property='og:image' content='https://cdn.acme.test/cap.jpg'>
            <meta property="product:price:amount" content="20.00"><meta property="product:price:currency" content="EUR">"#;
        let p = parse_page(og).unwrap();
        assert_eq!((p.title.as_deref(), p.price, p.currency.as_deref()), (Some("Cap & scarf"), Some(20.0), Some("EUR")));
        assert!(parse_page("<html><title>Blog</title><meta property=\"og:type\" content=\"article\"></html>").is_none());
        assert!(parse_page(r#"<script type="application/ld+json">{"@type":"Product","name":"X","image":"javascript:alert(1)"}</script>"#).unwrap().image_url.is_none());
    }
}
