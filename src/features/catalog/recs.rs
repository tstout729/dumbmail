//! Product lists and recommendations, computed in SQL from what people did (no trained model).
//!
//! - Best sellers: the most different buyers in the last N days.
//! - Bought together: how many people bought two products in the same order.
//! - Viewed together: how many people viewed both within 90 days.
//! - For a person: products bought or viewed together with theirs, then popular products in the
//!   categories they browse, leaving out what they've bought, then best sellers to fill.
//!
//! Only orders from a trusted source (the API, MCP or an integration) count as sales: anyone can
//! send events from a browser. Lists shown to people only hold verified products (see
//! `store::Product::verified`).
//!
//! Bought-together and viewed-together counts are cached per product for six hours (an empty
//! result for 15 minutes, so the first orders show up soon without recomputing for every email).

use std::collections::{HashMap, HashSet};

use serde_json::{json, Value};
use sqlx::PgPool;
use uuid::Uuid;

use super::store::{Product, PRODUCT_COLS};
use crate::error::AppResult;

const CACHE_HOURS: i32 = 6;
const EMPTY_CACHE_MINUTES: i32 = 15;
/// Products a person's picks start from.
const MAX_SEEDS: i64 = 12;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Related {
    BoughtTogether,
    ViewedTogether,
}

impl Related {
    fn key(self) -> &'static str {
        match self {
            Related::BoughtTogether => "bought_together",
            Related::ViewedTogether => "viewed_together",
        }
    }
}

/// Published, verified products in the order given (in stock only when asked): what lists shown to
/// anyone may hold.
pub async fn products_by_ids(db: &PgPool, account_id: Uuid, ids: &[Uuid], in_stock_only: bool) -> AppResult<Vec<Product>> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let q = format!(
        "SELECT {PRODUCT_COLS} FROM catalog_products cp WHERE cp.account_id = $1 AND cp.id = ANY($2) AND cp.published AND cp.verified AND (NOT $3 OR cp.in_stock)"
    );
    let rows: Vec<Product> = sqlx::query_as(&q).bind(account_id).bind(ids).bind(in_stock_only).fetch_all(db).await?;
    let mut by_id: HashMap<Uuid, Product> = rows.into_iter().map(|p| (p.id, p)).collect();
    Ok(ids.iter().filter_map(|id| by_id.remove(id)).collect())
}

/// Products by id whatever their state (unpublished and unverified too); the caller decides.
pub async fn products_by_ids_all(db: &PgPool, account_id: Uuid, ids: &[Uuid]) -> AppResult<HashMap<Uuid, Product>> {
    if ids.is_empty() {
        return Ok(HashMap::new());
    }
    let q = format!("SELECT {PRODUCT_COLS} FROM catalog_products cp WHERE cp.account_id = $1 AND cp.id = ANY($2)");
    let rows: Vec<Product> = sqlx::query_as(&q).bind(account_id).bind(ids).fetch_all(db).await?;
    Ok(rows.into_iter().map(|p| (p.id, p)).collect())
}

/// Products related to one product, with scores, best first.
pub async fn related(db: &PgPool, product_id: Uuid, kind: Related) -> AppResult<Vec<(Uuid, f64)>> {
    Ok(related_many(db, &[product_id], kind).await?.remove(&product_id).unwrap_or_default())
}

/// Products related to each of `seeds` (at most two queries to read the cache and compute what's
/// missing, whatever the number of seeds), best first.
pub async fn related_many(db: &PgPool, seeds: &[Uuid], kind: Related) -> AppResult<HashMap<Uuid, Vec<(Uuid, f64)>>> {
    let mut out: HashMap<Uuid, Vec<(Uuid, f64)>> = HashMap::new();
    if seeds.is_empty() {
        return Ok(out);
    }
    let cached: Vec<(Uuid, Value)> = sqlx::query_as(
        "SELECT product_id, items FROM catalog_rec_cache WHERE product_id = ANY($1) AND kind = $2
           AND computed_at > now() - CASE WHEN items = '[]'::jsonb THEN make_interval(mins => $4) ELSE make_interval(hours => $3) END",
    )
    .bind(seeds)
    .bind(kind.key())
    .bind(CACHE_HOURS)
    .bind(EMPTY_CACHE_MINUTES)
    .fetch_all(db)
    .await?;
    for (id, items) in cached {
        out.insert(id, serde_json::from_value(items).unwrap_or_default());
    }
    let missing: Vec<Uuid> = seeds.iter().filter(|s| !out.contains_key(s)).copied().collect::<HashSet<_>>().into_iter().collect();
    if missing.is_empty() {
        return Ok(out);
    }
    let rows: Vec<(Uuid, Uuid, f64)> = match kind {
        Related::BoughtTogether => {
            sqlx::query_as(
                "SELECT s.seed, r.product_id, r.n FROM unnest($1::uuid[]) s(seed), LATERAL (
                    SELECT b.product_id, count(DISTINCT b.event_id)::float8 AS n
                    FROM (SELECT event_id FROM catalog_interactions WHERE product_id = s.seed AND kind = 'ordered' AND trusted ORDER BY occurred_at DESC LIMIT 5000) a
                    JOIN catalog_interactions b ON b.event_id = a.event_id AND b.kind = 'ordered' AND b.trusted AND b.product_id <> s.seed
                    GROUP BY b.product_id ORDER BY 2 DESC, b.product_id LIMIT 20) r",
            )
            .bind(&missing)
            .fetch_all(db)
            .await?
        }
        Related::ViewedTogether => {
            sqlx::query_as(
                "SELECT s.seed, r.product_id, r.n FROM unnest($1::uuid[]) s(seed), LATERAL (
                    SELECT b.product_id, count(DISTINCT b.profile_id)::float8 AS n
                    FROM (SELECT DISTINCT profile_id FROM (SELECT profile_id FROM catalog_interactions
                            WHERE product_id = s.seed AND kind = 'viewed' AND occurred_at > now() - interval '90 days'
                            ORDER BY occurred_at DESC LIMIT 5000) x) a
                    JOIN catalog_interactions b ON b.profile_id = a.profile_id AND b.kind = 'viewed' AND b.product_id <> s.seed AND b.occurred_at > now() - interval '90 days'
                    GROUP BY b.product_id ORDER BY 2 DESC, b.product_id LIMIT 20) r",
            )
            .bind(&missing)
            .fetch_all(db)
            .await?
        }
    };
    for id in &missing {
        out.insert(*id, Vec::new());
    }
    for (seed, other, n) in rows {
        out.entry(seed).or_default().push((other, n));
    }
    let ids: Vec<Uuid> = missing.clone();
    let items: Vec<Value> = missing.iter().map(|m| json!(out.get(m).cloned().unwrap_or_default())).collect();
    sqlx::query(
        "INSERT INTO catalog_rec_cache (product_id, kind, items, computed_at) SELECT id, $3, items, now() FROM unnest($1::uuid[], $2::jsonb[]) AS t(id, items)
         ON CONFLICT (product_id, kind) DO UPDATE SET items = EXCLUDED.items, computed_at = now()",
    )
    .bind(&ids)
    .bind(&items)
    .bind(kind.key())
    .execute(db)
    .await?;
    Ok(out)
}

/// Best sellers: the most different buyers in trusted orders in the last `days` days (so one
/// shopper, or one made-up profile, can't push a product up); newest products fill in when there
/// are few orders.
pub async fn best_sellers(db: &PgPool, account_id: Uuid, days: i32, limit: i64, in_stock_only: bool, exclude: &[Uuid]) -> AppResult<Vec<Product>> {
    let q = format!(
        "SELECT {PRODUCT_COLS} FROM catalog_products cp
         LEFT JOIN (SELECT product_id, count(DISTINCT profile_id) AS units FROM catalog_interactions
                    WHERE account_id = $1 AND kind = 'ordered' AND trusted AND occurred_at > now() - make_interval(days => $2) GROUP BY product_id) s ON s.product_id = cp.id
         WHERE cp.account_id = $1 AND cp.published AND cp.verified AND (NOT $3 OR cp.in_stock) AND NOT (cp.id = ANY($4))
         ORDER BY COALESCE(s.units, 0) DESC, cp.published_at DESC, cp.id LIMIT $5"
    );
    Ok(sqlx::query_as(&q).bind(account_id).bind(days.clamp(1, 365)).bind(in_stock_only).bind(exclude).bind(limit).fetch_all(db).await?)
}

/// Products this person viewed, most recent first, each with the event of their latest view.
/// Unverified products are included: they're this person's own. Callers showing them to the person
/// use what their own event said (see `blocks`).
pub async fn recently_viewed(db: &PgPool, account_id: Uuid, profile_id: Uuid, limit: i64, in_stock_only: bool) -> AppResult<Vec<(Product, Uuid)>> {
    let rows: Vec<(Uuid, Uuid)> = sqlx::query_as(
        "SELECT product_id, event_id FROM (SELECT DISTINCT ON (product_id) product_id, event_id, occurred_at FROM catalog_interactions
            WHERE profile_id = $1 AND account_id = $2 AND kind = 'viewed' ORDER BY product_id, occurred_at DESC) x
         ORDER BY occurred_at DESC LIMIT $3",
    )
    .bind(profile_id)
    .bind(account_id)
    .bind(limit * 2)
    .fetch_all(db)
    .await?;
    let ids: Vec<Uuid> = rows.iter().map(|r| r.0).collect();
    let mut found = products_by_ids_all(db, account_id, &ids).await?;
    Ok(rows
        .into_iter()
        .filter_map(|(id, ev)| found.remove(&id).map(|p| (p, ev)))
        .filter(|(p, _)| p.published && (!in_stock_only || p.in_stock))
        .take(limit as usize)
        .collect())
}

/// Picks for one person. See the module docs for how they're chosen.
pub async fn for_person(db: &PgPool, account_id: Uuid, profile_id: Uuid, limit: i64) -> AppResult<Vec<Product>> {
    let limit = limit.clamp(1, 24);
    let bought: Vec<Uuid> = sqlx::query_scalar("SELECT DISTINCT product_id FROM catalog_interactions WHERE profile_id = $1 AND account_id = $2 AND kind = 'ordered'")
        .bind(profile_id)
        .bind(account_id)
        .fetch_all(db)
        .await?;
    let seeds: Vec<(Uuid, String)> = sqlx::query_as(
        "SELECT product_id, kind FROM catalog_interactions WHERE profile_id = $1 AND account_id = $2 AND occurred_at > now() - interval '180 days'
         GROUP BY product_id, kind ORDER BY max(occurred_at) DESC LIMIT $3",
    )
    .bind(profile_id)
    .bind(account_id)
    .bind(MAX_SEEDS)
    .fetch_all(db)
    .await?;
    let skip: HashSet<Uuid> = bought.iter().copied().collect();
    let mut scores: HashMap<Uuid, f64> = HashMap::new();
    let viewed: Vec<Uuid> = seeds.iter().filter(|s| s.1 == "viewed").map(|s| s.0).collect::<HashSet<_>>().into_iter().collect();
    let other: Vec<Uuid> = seeds.iter().filter(|s| s.1 != "viewed").map(|s| s.0).collect::<HashSet<_>>().into_iter().collect();
    for (rel, weight, list) in [(Related::ViewedTogether, 1.0, &viewed), (Related::BoughtTogether, 3.0, &other)] {
        for (_, rows) in related_many(db, list, rel).await? {
            let top = rows.first().map(|r| r.1).unwrap_or(1.0).max(1.0);
            for (o, n) in rows {
                if !skip.contains(&o) {
                    *scores.entry(o).or_default() += weight * n / top;
                }
            }
        }
    }
    // Popular products in the categories they look at most.
    let cats: Vec<String> = sqlx::query_scalar(
        "SELECT c FROM catalog_interactions ci JOIN catalog_products cp ON cp.id = ci.product_id, unnest(cp.categories) c
         WHERE ci.profile_id = $1 AND ci.account_id = $2 AND ci.occurred_at > now() - interval '180 days' AND cp.verified
         GROUP BY c ORDER BY count(*) DESC, c LIMIT 3",
    )
    .bind(profile_id)
    .bind(account_id)
    .fetch_all(db)
    .await?;
    if !cats.is_empty() {
        let rows: Vec<(Uuid, i64)> = sqlx::query_as(
            "SELECT cp.id, COALESCE((SELECT count(DISTINCT o.profile_id) FROM catalog_interactions o WHERE o.product_id = cp.id AND o.kind = 'ordered' AND o.trusted
                                     AND o.occurred_at > now() - interval '90 days'), 0)::bigint AS units
             FROM catalog_products cp WHERE cp.account_id = $1 AND cp.published AND cp.verified AND cp.in_stock AND cp.categories && $2
             ORDER BY units DESC, cp.published_at DESC LIMIT 30",
        )
        .bind(account_id)
        .bind(&cats)
        .fetch_all(db)
        .await?;
        let top = rows.first().map(|r| r.1).unwrap_or(0).max(1) as f64;
        for (id, units) in rows {
            if !skip.contains(&id) {
                *scores.entry(id).or_default() += 0.3 + 0.5 * units as f64 / top;
            }
        }
    }
    let mut ranked: Vec<(Uuid, f64)> = scores.into_iter().collect();
    ranked.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal).then(a.0.cmp(&b.0)));
    let ids: Vec<Uuid> = ranked.into_iter().map(|r| r.0).take(60).collect();
    let mut out = products_by_ids(db, account_id, &ids, true).await?;
    out.truncate(limit as usize);
    if (out.len() as i64) < limit {
        let mut exclude: Vec<Uuid> = bought.clone();
        exclude.extend(out.iter().map(|p| p.id));
        out.extend(best_sellers(db, account_id, 30, limit - out.len() as i64, true, &exclude).await?);
    }
    Ok(out)
}

/// Products related to one product: bought together first, then viewed together.
pub async fn for_product(db: &PgPool, account_id: Uuid, product_id: Uuid, limit: i64) -> AppResult<(Vec<Product>, Vec<Product>)> {
    let limit = limit.clamp(1, 24) as usize;
    let bought: Vec<Uuid> = related(db, product_id, Related::BoughtTogether).await?.into_iter().map(|r| r.0).collect();
    let viewed: Vec<Uuid> = related(db, product_id, Related::ViewedTogether).await?.into_iter().map(|r| r.0).collect();
    let mut b = products_by_ids(db, account_id, &bought, true).await?;
    let mut v = products_by_ids(db, account_id, &viewed, true).await?;
    b.truncate(limit);
    v.truncate(limit);
    Ok((b, v))
}
