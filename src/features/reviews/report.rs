//! Review reports: requests sent, how many people answered, average rating over time (days in the
//! account's time zone) and rating by product. Averages here count every review people wrote
//! (pending, published and rejected), not only the published ones.

use serde_json::{json, Value};
use sqlx::PgPool;
use uuid::Uuid;

use crate::error::AppResult;

pub async fn report(db: &PgPool, account_id: Uuid, tz: &str, days: i64) -> AppResult<Value> {
    let days = days.clamp(1, 730) as i32;
    let (sent, answered): (i64, i64) = sqlx::query_as(
        "SELECT count(*), count(*) FILTER (WHERE review_id IS NOT NULL) FROM review_requests
         WHERE account_id = $1 AND sent_at >= now() - make_interval(days => $2)",
    )
    .bind(account_id)
    .bind(days)
    .fetch_one(db)
    .await?;
    let statuses: Vec<(String, i64)> = sqlx::query_as(
        "SELECT status, count(*) FROM reviews WHERE account_id = $1 AND confirmed_at IS NOT NULL AND created_at >= now() - make_interval(days => $2) GROUP BY status",
    )
    .bind(account_id)
    .bind(days)
    .fetch_all(db)
    .await?;
    let awaiting: i64 = sqlx::query_scalar("SELECT count(*) FROM reviews WHERE account_id = $1 AND status = 'pending' AND confirmed_at IS NOT NULL").bind(account_id).fetch_one(db).await?;
    let series: Vec<(chrono::NaiveDate, i64, Option<f64>)> = sqlx::query_as(
        "SELECT (created_at AT TIME ZONE $3)::date AS day, count(*), avg(rating)::float8 FROM reviews
         WHERE account_id = $1 AND confirmed_at IS NOT NULL AND status <> 'removed' AND source <> 'import' AND created_at >= now() - make_interval(days => $2)
         GROUP BY 1 ORDER BY 1",
    )
    .bind(account_id)
    .bind(days)
    .bind(tz)
    .fetch_all(db)
    .await?;
    let (written, avg): (i64, Option<f64>) = sqlx::query_as(
        "SELECT count(*), avg(rating)::float8 FROM reviews WHERE account_id = $1 AND confirmed_at IS NOT NULL AND status <> 'removed' AND source <> 'import'
           AND created_at >= now() - make_interval(days => $2)",
    )
    .bind(account_id)
    .bind(days)
    .fetch_one(db)
    .await?;
    let products: Vec<(String, String, bool, i32, i64, i64, Option<f64>)> = sqlx::query_as(
        "SELECT s.external_id, s.name, s.name_trusted, s.review_count, s.rating_sum, count(r.id), avg(r.rating)::float8
         FROM review_subjects s LEFT JOIN reviews r ON r.subject_id = s.id AND r.confirmed_at IS NOT NULL AND r.status <> 'removed'
         WHERE s.account_id = $1 GROUP BY s.id ORDER BY count(r.id) DESC, s.name LIMIT 50",
    )
    .bind(account_id)
    .fetch_all(db)
    .await?;
    let round = |x: Option<f64>| x.map(|v| (v * 100.0).round() / 100.0);
    Ok(json!({
        "days": days,
        "requests_sent": sent,
        "requests_answered": answered,
        "response_rate": if sent > 0 { Some(answered as f64 / sent as f64) } else { None },
        "reviews_written": written,
        "average_rating": round(avg),
        "awaiting_moderation": awaiting,
        "by_status": statuses.into_iter().map(|(s, n)| (s, json!(n))).collect::<serde_json::Map<_, _>>(),
        "series": series.into_iter().map(|(d, n, a)| json!({"date": d, "reviews": n, "average_rating": round(a)})).collect::<Vec<_>>(),
        "products": products.into_iter().map(|(id, name, _trusted, pc, ps, n, a)| json!({
            "product_id": id, "name": name, "reviews": n, "average_rating": round(a),
            "published": pc, "published_average": if pc > 0 { round(Some(ps as f64 / f64::from(pc))) } else { None }
        })).collect::<Vec<_>>(),
    }))
}
