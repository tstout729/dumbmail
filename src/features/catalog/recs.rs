//! Product lists and recommendations, computed in SQL from what people did (no trained model).
//!
//! - Best sellers: units ordered in the last N days.
//! - Bought together: how often two products were in the same order.
//! - Viewed together: how many people viewed both within 90 days.
//! - For a person: products bought or viewed together with theirs, then popular products in the
//!   categories they browse, leaving out what they've bought, then best sellers to fill.
//!
//! Bought-together and viewed-together counts are cached per product for six hours.

use std::collections::{HashMap, HashSet};

use serde_json::{json, Value};
use sqlx::PgPool;
use uuid::Uuid;

use super::store::{Product, PRODUCT_COLS};
use crate::error::AppResult;

const CACHE_HOURS: i64 = 6;

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

/// Products in the order given, skipping unpublished ones (and sold-out ones when asked).
pub async fn products_by_ids(db: &PgPool, account_id: Uuid, ids: &[Uuid], in_stock_only: bool) -> AppResult<Vec<Product>> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let q = format!(
        "SELECT {PRODUCT_COLS} FROM catalog_products cp WHERE cp.account_id = $1 AND cp.id = ANY($2) AND cp.published AND (NOT $3 OR cp.in_stock)"
    );
    let rows: Vec<Product> = sqlx::query_as(&q).bind(account_id).bind(ids).bind(in_stock_only).fetch_all(db).await?;
    let mut by_id: HashMap<Uuid, Product> = rows.into_iter().map(|p| (p.id, p)).collect();
    Ok(ids.iter().filter_map(|id| by_id.remove(id)).collect())
}

/// Products related to one product, with scores, best first.
pub async fn related(db: &PgPool, product_id: Uuid, kind: Related) -> AppResult<Vec<(Uuid, f64)>> {
    let cached: Option<Value> = sqlx::query_scalar(
        "SELECT items FROM catalog_rec_cache WHERE product_id = $1 AND kind = $2 AND computed_at > now() - make_interval(hours => $3)",
    )
    .bind(product_id)
    .bind(kind.key())
    .bind(CACHE_HOURS as i32)
    .fetch_optional(db)
    .await?;
    if let Some(v) = cached {
        return Ok(serde_json::from_value(v).unwrap_or_default());
    }
    let rows: Vec<(Uuid, f64)> = match kind {
        Related::BoughtTogether => {
            sqlx::query_as(
                "SELECT b.product_id, count(DISTINCT b.event_id)::float8
                 FROM (SELECT event_id FROM catalog_interactions WHERE product_id = $1 AND kind = 'ordered' ORDER BY occurred_at DESC LIMIT 5000) a
                 JOIN catalog_interactions b ON b.event_id = a.event_id AND b.kind = 'ordered' AND b.product_id <> $1
                 GROUP BY b.product_id ORDER BY 2 DESC, b.product_id LIMIT 20",
            )
            .bind(product_id)
            .fetch_all(db)
            .await?
        }
        Related::ViewedTogether => {
            sqlx::query_as(
                "WITH a AS (SELECT DISTINCT profile_id FROM (SELECT profile_id FROM catalog_interactions
                              WHERE product_id = $1 AND kind = 'viewed' AND occurred_at > now() - interval '90 days'
                              ORDER BY occurred_at DESC LIMIT 5000) x)
                 SELECT b.product_id, count(DISTINCT b.profile_id)::float8 FROM a
                 JOIN catalog_interactions b ON b.profile_id = a.profile_id AND b.kind = 'viewed' AND b.product_id <> $1 AND b.occurred_at > now() - interval '90 days'
                 GROUP BY b.product_id ORDER BY 2 DESC, b.product_id LIMIT 20",
            )
            .bind(product_id)
            .fetch_all(db)
            .await?
        }
    };
    sqlx::query(
        "INSERT INTO catalog_rec_cache (product_id, kind, items, computed_at) VALUES ($1, $2, $3, now())
         ON CONFLICT (product_id, kind) DO UPDATE SET items = EXCLUDED.items, computed_at = now()",
    )
    .bind(product_id)
    .bind(kind.key())
    .bind(json!(rows))
    .execute(db)
    .await?;
    Ok(rows)
}

/// Best sellers by units ordered in the last `days` days; newest products fill in when there are few orders.
pub async fn best_sellers(db: &PgPool, account_id: Uuid, days: i32, limit: i64, in_stock_only: bool, exclude: &[Uuid]) -> AppResult<Vec<Product>> {
    let q = format!(
        "SELECT {PRODUCT_COLS} FROM catalog_products cp
         LEFT JOIN (SELECT product_id, sum(quantity) AS units FROM catalog_interactions
                    WHERE account_id = $1 AND kind = 'ordered' AND occurred_at > now() - make_interval(days => $2) GROUP BY product_id) s ON s.product_id = cp.id
         WHERE cp.account_id = $1 AND cp.published AND (NOT $3 OR cp.in_stock) AND NOT (cp.id = ANY($4))
         ORDER BY COALESCE(s.units, 0) DESC, cp.published_at DESC, cp.id LIMIT $5"
    );
    Ok(sqlx::query_as(&q).bind(account_id).bind(days.clamp(1, 365)).bind(in_stock_only).bind(exclude).bind(limit).fetch_all(db).await?)
}

/// Products this person viewed, most recent first.
pub async fn recently_viewed(db: &PgPool, account_id: Uuid, profile_id: Uuid, limit: i64, in_stock_only: bool) -> AppResult<Vec<Product>> {
    let ids: Vec<Uuid> = sqlx::query_scalar(
        "SELECT product_id FROM catalog_interactions WHERE profile_id = $1 AND account_id = $2 AND kind = 'viewed'
         GROUP BY product_id ORDER BY max(occurred_at) DESC LIMIT $3",
    )
    .bind(profile_id)
    .bind(account_id)
    .bind(limit * 2)
    .fetch_all(db)
    .await?;
    let mut out = products_by_ids(db, account_id, &ids, in_stock_only).await?;
    out.truncate(limit as usize);
    Ok(out)
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
         GROUP BY product_id, kind ORDER BY max(occurred_at) DESC LIMIT 12",
    )
    .bind(profile_id)
    .bind(account_id)
    .fetch_all(db)
    .await?;
    let skip: HashSet<Uuid> = bought.iter().copied().collect();
    let mut scores: HashMap<Uuid, f64> = HashMap::new();
    let mut seen_seed = HashSet::new();
    for (pid, kind) in &seeds {
        if !seen_seed.insert((*pid, kind == "viewed")) {
            continue;
        }
        let (rel, weight) = if kind == "viewed" { (Related::ViewedTogether, 1.0) } else { (Related::BoughtTogether, 3.0) };
        let rows = related(db, *pid, rel).await?;
        let top = rows.first().map(|r| r.1).unwrap_or(1.0).max(1.0);
        for (other, n) in rows {
            if !skip.contains(&other) {
                *scores.entry(other).or_default() += weight * n / top;
            }
        }
    }
    // Popular products in the categories they look at most.
    let cats: Vec<String> = sqlx::query_scalar(
        "SELECT c FROM catalog_interactions ci JOIN catalog_products cp ON cp.id = ci.product_id, unnest(cp.categories) c
         WHERE ci.profile_id = $1 AND ci.account_id = $2 AND ci.occurred_at > now() - interval '180 days'
         GROUP BY c ORDER BY count(*) DESC, c LIMIT 3",
    )
    .bind(profile_id)
    .bind(account_id)
    .fetch_all(db)
    .await?;
    if !cats.is_empty() {
        let rows: Vec<(Uuid, i64)> = sqlx::query_as(
            "SELECT cp.id, COALESCE((SELECT sum(quantity) FROM catalog_interactions o WHERE o.product_id = cp.id AND o.kind = 'ordered'
                                     AND o.occurred_at > now() - interval '90 days'), 0)::bigint AS units
             FROM catalog_products cp WHERE cp.account_id = $1 AND cp.published AND cp.in_stock AND cp.categories && $2
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
