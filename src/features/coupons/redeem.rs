//! Redemptions, learned from orders by a reconciler, a scheduler step that can't lose an event:
//! it reads "Placed Order" events of accounts with coupons newer than a stored cursor (event ids are
//! time-ordered), re-reads a short overlap for events that committed late, and skips events it has
//! already handled, so each order is matched once however often it's seen.
//!
//! An order's discount code redeems exactly one assignment: the same person's, with the same code
//! (case-insensitive), given before the order and not expired then; the most recently given if
//! several match. An order's revenue is credited once, to its first redeemed code.
//!
//! Orders recorded by the website script (`events.from_browser`) could be made up by a visitor:
//! they're kept as unverified evidence in their own columns, never count as redeemed (segments,
//! revenue), and never stop a later trusted order (API, MCP, a store integration) from redeeming
//! the same code. Orders never change when a code counts as given.

use chrono::{DateTime, Utc};
use serde_json::{json, Value};
use sqlx::PgPool;
use uuid::Uuid;

use super::store::{self, Coupon};
use crate::error::AppResult;

pub const ORDER_METRIC: &str = "Placed Order";
/// Most codes read from one order.
const MAX_CODES_PER_ORDER: usize = 20;
/// Order events handled per reconciler run.
const BATCH: i64 = 2_000;
/// How far behind the cursor each run looks again, for events that committed late.
const OVERLAP_SECS: i64 = 300;
/// How long handled events are remembered (well past the overlap).
const SEEN_DAYS: i32 = 2;
/// Longest a report query may run.
const REPORT_TIMEOUT: &str = "5s";

/// The discount codes an order event carries, upper-cased: `discount_codes` (strings, or objects
/// with `code`, as the website script and Shopify send them) and `discount_code`. Anything that
/// isn't a valid code is ignored.
pub fn order_codes(props: &Value) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut push = |v: &Value| {
        let s = v.as_str().or_else(|| v.get("code").and_then(Value::as_str)).map(str::trim).unwrap_or("");
        if let Some(s) = store::normalize(s) {
            if out.len() < MAX_CODES_PER_ORDER && !out.contains(&s) {
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

/// The smallest v7 id made at `t`, for "events at or after t".
fn id_at(t: DateTime<Utc>) -> Uuid {
    let ms = t.timestamp_millis().max(0) as u64;
    let mut b = [0u8; 16];
    b[..6].copy_from_slice(&ms.to_be_bytes()[2..]);
    b[6] = 0x70;
    b[8] = 0x80;
    Uuid::from_bytes(b)
}

fn time_of(id: Uuid) -> DateTime<Utc> {
    let b = id.as_bytes();
    let ms = u64::from_be_bytes([0, 0, b[0], b[1], b[2], b[3], b[4], b[5]]);
    DateTime::from_timestamp_millis(ms as i64).unwrap_or_else(Utc::now)
}

/// Scheduler: match new order events to codes. Returns how many order events it handled. One
/// server runs it at a time.
pub async fn reconcile(db: &PgPool) -> anyhow::Result<usize> {
    let mut lock = db.begin().await?;
    let got: bool = sqlx::query_scalar("SELECT pg_try_advisory_xact_lock(hashtextextended('coupons-reconcile', 0))").fetch_one(&mut *lock).await?;
    if !got {
        return Ok(0);
    }
    // Coupons didn't exist before the first run: start a little before it.
    let cursor: Uuid = sqlx::query_scalar(
        "INSERT INTO coupon_reconcile (id, cursor_id) VALUES (1, $1) ON CONFLICT (id) DO UPDATE SET id = 1 RETURNING cursor_id",
    )
    .bind(id_at(Utc::now() - chrono::Duration::hours(1)))
    .fetch_one(&mut *lock)
    .await?;
    let from = id_at(time_of(cursor) - chrono::Duration::seconds(OVERLAP_SECS));
    #[allow(clippy::type_complexity)]
    let rows: Vec<(Uuid, Uuid, Uuid, Value, Option<f64>, DateTime<Utc>, bool)> = sqlx::query_as(
        "SELECT e.id, e.account_id, e.profile_id, e.properties, e.value, e.occurred_at, COALESCE(e.from_browser, false)
         FROM events e JOIN metrics m ON m.id = e.metric_id
         WHERE e.id > $1 AND lower(m.name) = lower($2)
           AND EXISTS (SELECT 1 FROM coupons c WHERE c.account_id = e.account_id AND c.deleted_at IS NULL)
           AND NOT EXISTS (SELECT 1 FROM coupon_orders_seen s WHERE s.event_id = e.id)
         ORDER BY e.id LIMIT $3",
    )
    .bind(from)
    .bind(ORDER_METRIC)
    .bind(BATCH)
    .fetch_all(&mut *lock)
    .await?;
    let mut last = cursor;
    for (event_id, account_id, profile_id, props, value, at, from_browser) in &rows {
        match_order(db, *event_id, *account_id, *profile_id, props, *value, *at, *from_browser).await?;
        last = last.max(*event_id);
    }
    // With no order events, still move past everything recorded since (up to now, less the overlap).
    if rows.is_empty() {
        last = last.max(id_at(Utc::now() - chrono::Duration::seconds(OVERLAP_SECS)));
    }
    sqlx::query("UPDATE coupon_reconcile SET cursor_id = $1, updated_at = now() WHERE id = 1").bind(last).execute(&mut *lock).await?;
    sqlx::query("DELETE FROM coupon_orders_seen WHERE seen_at < now() - make_interval(days => $1)").bind(SEEN_DAYS).execute(&mut *lock).await?;
    lock.commit().await?;
    Ok(rows.len())
}

/// Match one order event (once: the "seen" row and the matches commit together).
#[allow(clippy::too_many_arguments)]
async fn match_order(db: &PgPool, event_id: Uuid, account_id: Uuid, profile_id: Uuid, props: &Value, value: Option<f64>, at: DateTime<Utc>, from_browser: bool) -> anyhow::Result<()> {
    let mut tx = db.begin().await?;
    let first = sqlx::query("INSERT INTO coupon_orders_seen (event_id) VALUES ($1) ON CONFLICT DO NOTHING").bind(event_id).execute(&mut *tx).await?.rows_affected();
    if first == 0 {
        return Ok(());
    }
    let mut credited = false;
    for code in order_codes(props) {
        let found: Option<i64> = sqlx::query_scalar(
            "SELECT cc.id FROM coupon_codes cc JOIN coupons c ON c.id = cc.coupon_id AND c.deleted_at IS NULL
             WHERE cc.profile_id = $1 AND cc.code_norm = $2 AND cc.account_id = $3 AND cc.status = 'assigned'
               AND cc.assigned_at <= $4 AND (cc.expires_at IS NULL OR cc.expires_at > $4)
             ORDER BY cc.assigned_at DESC LIMIT 1 FOR UPDATE OF cc",
        )
        .bind(profile_id)
        .bind(&code)
        .bind(account_id)
        .bind(at)
        .fetch_optional(&mut *tx)
        .await?;
        let Some(id) = found else { continue };
        if from_browser {
            sqlx::query("UPDATE coupon_codes SET browser_order_event_id = $2, browser_order_at = $3, browser_order_value = $4 WHERE id = $1 AND browser_order_at IS NULL")
                .bind(id)
                .bind(event_id)
                .bind(at)
                .bind(value)
                .execute(&mut *tx)
                .await?;
            continue;
        }
        let credit = !credited && value.is_some();
        let n = sqlx::query("UPDATE coupon_codes SET redeemed_at = $2, order_event_id = $3, order_value = CASE WHEN $4 THEN $5 END WHERE id = $1 AND redeemed_at IS NULL")
            .bind(id)
            .bind(at)
            .bind(event_id)
            .bind(credit)
            .bind(value)
            .execute(&mut *tx)
            .await?
            .rows_affected();
        credited |= n > 0 && credit;
    }
    tx.commit().await?;
    Ok(())
}

/// Codes given (that reached the person), redeemed, redemption rate and revenue for one coupon,
/// with the last `days` by day in the account's time zone. Indexed, grouped, and time-limited.
pub async fn report(db: &PgPool, c: &Coupon, days: i64) -> AppResult<Value> {
    let days = days.clamp(1, 365);
    let mut tx = db.begin().await?;
    sqlx::query(&format!("SET LOCAL statement_timeout = '{REPORT_TIMEOUT}'")).execute(&mut *tx).await?;
    let tz: String = sqlx::query_scalar("SELECT timezone FROM accounts WHERE id = $1").bind(c.account_id).fetch_one(&mut *tx).await?;
    let (given, redeemed, unverified, revenue): (i64, i64, i64, f64) = sqlx::query_as(
        "SELECT count(*) FILTER (WHERE delivered_at IS NOT NULL), count(*) FILTER (WHERE redeemed_at IS NOT NULL),
                count(*) FILTER (WHERE redeemed_at IS NULL AND browser_order_at IS NOT NULL),
                COALESCE(sum(order_value), 0)::float8
         FROM coupon_codes WHERE coupon_id = $1 AND status = 'assigned'",
    )
    .bind(c.id)
    .fetch_one(&mut *tx)
    .await?;
    let series: Vec<(String, i64, i64, f64)> = sqlx::query_as(
        "WITH bounds AS (SELECT ((now() AT TIME ZONE $2)::date - ($3::int - 1)) AS first_day),
              since AS (SELECT (first_day::timestamp AT TIME ZONE $2) AS t, first_day FROM bounds),
              x AS (
                SELECT (delivered_at AT TIME ZONE $2)::date AS day, 1 AS g, 0 AS r, 0::float8 AS v
                  FROM coupon_codes, since WHERE coupon_id = $1 AND delivered_at >= since.t
                UNION ALL
                SELECT (redeemed_at AT TIME ZONE $2)::date, 0, 1, COALESCE(order_value, 0)
                  FROM coupon_codes, since WHERE coupon_id = $1 AND redeemed_at >= since.t)
         SELECT to_char(day, 'YYYY-MM-DD'), sum(g)::bigint, sum(r)::bigint, sum(v)::float8 FROM x GROUP BY day",
    )
    .bind(c.id)
    .bind(&tz)
    .bind(days as i32)
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    let tzp: chrono_tz::Tz = tz.parse().unwrap_or(chrono_tz::America::Los_Angeles);
    let today = Utc::now().with_timezone(&tzp).date_naive();
    let by_day: std::collections::HashMap<String, (i64, i64, f64)> = series.into_iter().map(|(d, g, r, v)| (d, (g, r, v))).collect();
    let days_out: Vec<Value> = (0..days)
        .rev()
        .map(|back| {
            let d = (today - chrono::Duration::days(back)).format("%Y-%m-%d").to_string();
            let (g, r, v) = by_day.get(&d).copied().unwrap_or((0, 0, 0.0));
            json!({"date": d, "given": g, "redeemed": r, "revenue": v})
        })
        .collect();
    let stock = store::stock(db, c).await?;
    Ok(json!({
        "coupon": {"id": c.id, "name": c.name},
        "given": given,
        "redeemed": redeemed,
        "redeemed_unverified": unverified,
        "redemption_rate": if given > 0 { redeemed as f64 / given as f64 } else { 0.0 },
        "revenue": revenue,
        "stock": stock,
        "days": days_out,
        "note": "Redemptions come from Placed Order events that carry the code, matched to the person it was given to, within a minute or two. Orders recorded by the website script could be made up by a visitor, so they count as unverified and add no revenue; orders from the API or a store integration count fully. Each order's revenue counts once.",
    }))
}

/// The same totals for every coupon (no daily breakdown).
pub async fn summary(db: &PgPool, account_id: Uuid) -> AppResult<Value> {
    let mut tx = db.begin().await?;
    sqlx::query(&format!("SET LOCAL statement_timeout = '{REPORT_TIMEOUT}'")).execute(&mut *tx).await?;
    let rows: Vec<(Uuid, String, i64, i64, i64, i64, f64)> = sqlx::query_as(
        "SELECT c.id, c.name, c.codes_total,
                count(cc.id) FILTER (WHERE cc.delivered_at IS NOT NULL),
                count(cc.id) FILTER (WHERE cc.redeemed_at IS NOT NULL),
                count(cc.id) FILTER (WHERE cc.redeemed_at IS NULL AND cc.browser_order_at IS NOT NULL),
                COALESCE(sum(cc.order_value), 0)::float8
         FROM coupons c LEFT JOIN coupon_codes cc ON cc.coupon_id = c.id AND cc.status = 'assigned'
         WHERE c.account_id = $1 AND c.deleted_at IS NULL GROUP BY c.id ORDER BY c.created_at DESC LIMIT 500",
    )
    .bind(account_id)
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
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
        let p = json!({"discount_codes": ["save-1", {"code": "SAVE-2", "amount": 5}, "", "save-1", "straße"], "discount_code": "Extra"});
        assert_eq!(order_codes(&p), vec!["SAVE-1", "SAVE-2", "EXTRA"]);
        assert!(order_codes(&json!({"discount_codes": "not-a-list"})).is_empty());
        let many: Vec<String> = (0..50).map(|i| format!("CODE{i}")).collect();
        assert_eq!(order_codes(&json!({"discount_codes": many})).len(), MAX_CODES_PER_ORDER);
    }

    #[test]
    fn cursor_ids_follow_time() {
        let t = Utc::now();
        let a = id_at(t);
        assert!(a <= Uuid::now_v7());
        assert_eq!(time_of(a).timestamp_millis(), t.timestamp_millis());
        assert!(id_at(t - chrono::Duration::seconds(1)) < a);
    }
}
