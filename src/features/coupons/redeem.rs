//! Redemptions, learned from orders: a "Placed Order" event whose discount codes include a code
//! given to that same person marks it redeemed and links the order. Matching only the person's own
//! codes means a made-up order (the website script's endpoint takes any event) can't mark someone
//! else's code redeemed; redemption numbers are still only as trustworthy as the order source.
//!
//! Orders recorded through the website script (`events.from_browser`) could be made up by a
//! visitor, so they mark a code "redeemed (unverified)": shown in the code list and the report's
//! unverified count, but left out of revenue and of the "redeemed coupon" segment rule. An order
//! recorded by the API, MCP or a store integration verifies the code (and takes over the link).

use chrono::{DateTime, Utc};
use serde_json::{json, Value};
use sqlx::PgPool;
use uuid::Uuid;

use super::store::{self, Coupon, MAX_CODE_LEN};
use crate::error::AppResult;

pub const ORDER_METRIC: &str = "Placed Order";
/// Most codes read from one order.
const MAX_CODES_PER_ORDER: usize = 20;

/// The discount codes an order event carries, upper-cased: `discount_codes` (strings, or objects
/// with `code`, as the website script and Shopify send them) and `discount_code`.
pub fn order_codes(props: &Value) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut push = |v: &Value| {
        let s = v.as_str().or_else(|| v.get("code").and_then(Value::as_str)).map(str::trim).unwrap_or("");
        if !s.is_empty() && s.chars().count() <= MAX_CODE_LEN && out.len() < MAX_CODES_PER_ORDER {
            let s = s.to_uppercase();
            if !out.contains(&s) {
                out.push(s);
            }
        }
    };
    if let Some(a) = props.get("discount_codes").and_then(Value::as_array) {
        a.iter().take(MAX_CODES_PER_ORDER).for_each(&mut push);
    }
    if let Some(v) = props.get("discount_code") {
        push(v);
    }
    out
}

/// Whether the account has any coupons (cheap; checked for each order event).
pub async fn account_has_coupons(db: &PgPool, account_id: Uuid) -> bool {
    sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM coupons WHERE account_id = $1 AND deleted_at IS NULL)")
        .bind(account_id)
        .fetch_one(db)
        .await
        .unwrap_or(false)
}

/// Mark the codes an order used. Safe to run twice for the same event: a redeemed code stays with its first order.
pub async fn on_order(db: &PgPool, event_id: Uuid) -> anyhow::Result<u64> {
    let row: Option<(Uuid, Uuid, String, Value, Option<f64>, DateTime<Utc>, bool)> = sqlx::query_as(
        "SELECT e.account_id, e.profile_id, m.name, e.properties, e.value, e.occurred_at, e.from_browser IS NOT FALSE
         FROM events e JOIN metrics m ON m.id = e.metric_id WHERE e.id = $1",
    )
    .bind(event_id)
    .fetch_optional(db)
    .await?;
    let Some((account_id, profile_id, metric, props, value, at, from_browser)) = row else { return Ok(0) };
    if !metric.eq_ignore_ascii_case(ORDER_METRIC) {
        return Ok(0);
    }
    let codes = order_codes(&props);
    if codes.is_empty() {
        return Ok(0);
    }
    // A browser order only claims a code nobody used yet; a trusted order also verifies one a browser claimed.
    Ok(sqlx::query(
        "UPDATE coupon_codes cc SET status = 'redeemed', redeemed_at = $4, order_event_id = $5, order_value = $6, redeemed_verified = NOT $7,
                delivered_at = COALESCE(cc.delivered_at, $4)
         WHERE cc.account_id = $1 AND cc.profile_id = $2 AND cc.code_norm = ANY($3)
           AND (cc.status = 'assigned' OR (NOT $7 AND cc.status = 'redeemed' AND cc.redeemed_verified IS NOT TRUE))
           AND EXISTS (SELECT 1 FROM coupons c WHERE c.id = cc.coupon_id AND c.deleted_at IS NULL)",
    )
    .bind(account_id)
    .bind(profile_id)
    .bind(&codes)
    .bind(at)
    .bind(event_id)
    .bind(value)
    .bind(from_browser)
    .execute(db)
    .await?
    .rows_affected())
}

/// Codes given (that reached the person), redeemed, redemption rate and revenue for one coupon,
/// with the last `days` by day in the account's time zone.
pub async fn report(db: &PgPool, c: &Coupon, days: i64) -> AppResult<Value> {
    let days = days.clamp(1, 365);
    let (given, redeemed, unverified, revenue): (i64, i64, i64, f64) = sqlx::query_as(
        "SELECT count(*) FILTER (WHERE delivered_at IS NOT NULL), count(*) FILTER (WHERE status = 'redeemed' AND redeemed_verified),
                count(*) FILTER (WHERE status = 'redeemed' AND redeemed_verified IS NOT TRUE),
                COALESCE(sum(order_value) FILTER (WHERE status = 'redeemed' AND redeemed_verified), 0)::float8
         FROM coupon_codes WHERE coupon_id = $1 AND status <> 'available'",
    )
    .bind(c.id)
    .fetch_one(db)
    .await?;
    let tz: String = sqlx::query_scalar("SELECT timezone FROM accounts WHERE id = $1").bind(c.account_id).fetch_one(db).await?;
    let series: Vec<(String, i64, i64, f64)> = sqlx::query_as(
        "WITH d AS (SELECT generate_series((now() AT TIME ZONE $2)::date - ($3::int - 1), (now() AT TIME ZONE $2)::date, interval '1 day')::date AS day)
         SELECT to_char(d.day, 'YYYY-MM-DD'),
                (SELECT count(*) FROM coupon_codes g WHERE g.coupon_id = $1 AND g.status <> 'available' AND (g.delivered_at AT TIME ZONE $2)::date = d.day),
                (SELECT count(*) FROM coupon_codes r WHERE r.coupon_id = $1 AND r.status = 'redeemed' AND r.redeemed_verified AND (r.redeemed_at AT TIME ZONE $2)::date = d.day),
                (SELECT COALESCE(sum(r.order_value), 0)::float8 FROM coupon_codes r WHERE r.coupon_id = $1 AND r.status = 'redeemed' AND r.redeemed_verified AND (r.redeemed_at AT TIME ZONE $2)::date = d.day)
         FROM d ORDER BY d.day",
    )
    .bind(c.id)
    .bind(&tz)
    .bind(days as i32)
    .fetch_all(db)
    .await?;
    let stock = store::stock(db, c).await?;
    Ok(json!({
        "coupon": {"id": c.id, "name": c.name},
        "given": given,
        "redeemed": redeemed,
        "redeemed_unverified": unverified,
        "redemption_rate": if given > 0 { redeemed as f64 / given as f64 } else { 0.0 },
        "revenue": revenue,
        "stock": stock,
        "days": series.into_iter().map(|(day, g, r, rev)| json!({"date": day, "given": g, "redeemed": r, "revenue": rev})).collect::<Vec<_>>(),
        "note": "Redemptions come from Placed Order events that carry the code, matched to the person it was given to. Orders recorded by the website script could be made up by a visitor, so they count as unverified and add no revenue; orders from the API or a store integration count fully. These numbers are only as reliable as where your orders come from.",
    }))
}

/// The same totals for every coupon (no daily breakdown).
pub async fn summary(db: &PgPool, account_id: Uuid) -> AppResult<Value> {
    let rows: Vec<(Uuid, String, i64, i64, i64, i64, f64)> = sqlx::query_as(
        "SELECT c.id, c.name, c.codes_total,
                count(cc.id) FILTER (WHERE cc.delivered_at IS NOT NULL),
                count(cc.id) FILTER (WHERE cc.status = 'redeemed' AND cc.redeemed_verified),
                count(cc.id) FILTER (WHERE cc.status = 'redeemed' AND cc.redeemed_verified IS NOT TRUE),
                COALESCE(sum(cc.order_value) FILTER (WHERE cc.status = 'redeemed' AND cc.redeemed_verified), 0)::float8
         FROM coupons c LEFT JOIN coupon_codes cc ON cc.coupon_id = c.id AND cc.status <> 'available'
         WHERE c.account_id = $1 AND c.deleted_at IS NULL GROUP BY c.id ORDER BY c.created_at DESC LIMIT 500",
    )
    .bind(account_id)
    .fetch_all(db)
    .await?;
    Ok(json!({"data": rows.into_iter().map(|(id, name, total, given, redeemed, unverified, revenue)| json!({
        "id": id, "name": name, "codes_total": total, "given": given, "redeemed": redeemed, "redeemed_unverified": unverified,
        "redemption_rate": if given > 0 { redeemed as f64 / given as f64 } else { 0.0 }, "revenue": revenue,
    })).collect::<Vec<_>>()}))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_codes_from_orders() {
        let p = json!({"discount_codes": ["save-1", {"code": "SAVE-2", "amount": 5}, "", "save-1"], "discount_code": "Extra"});
        assert_eq!(order_codes(&p), vec!["SAVE-1", "SAVE-2", "EXTRA"]);
        assert!(order_codes(&json!({"discount_codes": "not-a-list"})).is_empty());
        let many: Vec<String> = (0..50).map(|i| format!("C{i}")).collect();
        assert_eq!(order_codes(&json!({"discount_codes": many})).len(), MAX_CODES_PER_ORDER);
    }
}
