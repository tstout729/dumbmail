use axum::extract::{Query, State};
use axum::routing::get;
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};
use uuid::Uuid;

use crate::auth::Ctx;
use crate::campaigns::ATTRIBUTION_SQL;
use crate::error::AppResult;
use crate::state::AppState;

pub fn routes() -> Router<AppState> {
    Router::new().route("/api/analytics/overview", get(overview_h))
}

#[derive(Deserialize)]
struct Days {
    days: Option<i64>,
}

/// Dashboard numbers for the last `days` days, bucketed by day in the account's time zone.
pub async fn overview(state: &AppState, account_id: Uuid, days: i64) -> AppResult<Value> {
    let days = days.clamp(1, 365);
    let tz: String = sqlx::query_scalar("SELECT timezone FROM accounts WHERE id = $1").bind(account_id).fetch_one(&state.db).await?;
    let since = format!("now() - interval '{days} days'");

    let (sent_email, sent_sms, delivered, opens, clicks, unsubs, bounces): (i64, i64, i64, i64, i64, i64, i64) = sqlx::query_as(&format!(
        "SELECT count(*) FILTER (WHERE channel = 'email' AND status NOT IN ('queued','failed','skipped')),
                count(*) FILTER (WHERE channel = 'sms' AND status NOT IN ('queued','failed','skipped')),
                count(*) FILTER (WHERE channel = 'email' AND status IN ('sent','delivered')),
                count(opened_at) FILTER (WHERE channel = 'email'),
                count(clicked_at),
                count(unsubscribed_at),
                count(*) FILTER (WHERE status = 'bounced')
         FROM messages WHERE account_id = $1 AND NOT is_test AND created_at >= {since}"
    ))
    .bind(account_id)
    .fetch_one(&state.db)
    .await?;

    let (revenue, attributed, campaign_rev, flow_rev): (f64, f64, f64, f64) = sqlx::query_as(&format!(
        "SELECT COALESCE(sum(value), 0)::float8,
                COALESCE(sum(value) FILTER (WHERE campaign_id IS NOT NULL OR flow_id IS NOT NULL), 0)::float8,
                COALESCE(sum(value) FILTER (WHERE campaign_id IS NOT NULL), 0)::float8,
                COALESCE(sum(value) FILTER (WHERE flow_id IS NOT NULL), 0)::float8
         FROM ({ATTRIBUTION_SQL} AND e.occurred_at >= {since}) x"
    ))
    .bind(account_id)
    .fetch_one(&state.db)
    .await?;

    let (profiles, new_profiles): (i64, i64) = sqlx::query_as(&format!(
        "SELECT count(*), count(*) FILTER (WHERE created_at >= {since}) FROM profiles WHERE account_id = $1"
    ))
    .bind(account_id)
    .fetch_one(&state.db)
    .await?;

    let rows: Vec<(String, f64, f64, i64, i64, i64, i64)> = sqlx::query_as(&format!(
        "WITH d AS (
            SELECT to_char(g, 'YYYY-MM-DD') AS day FROM generate_series(
              date_trunc('day', ({since}) AT TIME ZONE $2), date_trunc('day', now() AT TIME ZONE $2), interval '1 day') g),
         m AS (
            SELECT to_char(created_at AT TIME ZONE $2, 'YYYY-MM-DD') AS day,
                   count(*) FILTER (WHERE channel = 'email' AND status NOT IN ('queued','failed','skipped')) AS emails,
                   count(*) FILTER (WHERE channel = 'sms' AND status NOT IN ('queued','failed','skipped')) AS sms
            FROM messages WHERE account_id = $1 AND NOT is_test AND created_at >= {since} GROUP BY 1),
         o AS (SELECT to_char(opened_at AT TIME ZONE $2, 'YYYY-MM-DD') AS day, count(*) AS n FROM messages WHERE account_id = $1 AND NOT is_test AND opened_at >= {since} GROUP BY 1),
         c AS (SELECT to_char(clicked_at AT TIME ZONE $2, 'YYYY-MM-DD') AS day, count(*) AS n FROM messages WHERE account_id = $1 AND NOT is_test AND clicked_at >= {since} GROUP BY 1),
         r AS (SELECT to_char(occurred_at AT TIME ZONE $2, 'YYYY-MM-DD') AS day, sum(value) AS rev,
                      sum(value) FILTER (WHERE campaign_id IS NOT NULL OR flow_id IS NOT NULL) AS att
               FROM ({ATTRIBUTION_SQL} AND e.occurred_at >= {since}) x GROUP BY 1)
         SELECT d.day, COALESCE(r.rev, 0)::float8, COALESCE(r.att, 0)::float8, COALESCE(m.emails, 0), COALESCE(m.sms, 0), COALESCE(o.n, 0), COALESCE(c.n, 0)
         FROM d LEFT JOIN m USING (day) LEFT JOIN o USING (day) LEFT JOIN c USING (day) LEFT JOIN r USING (day) ORDER BY d.day"
    ))
    .bind(account_id)
    .bind(&tz)
    .fetch_all(&state.db)
    .await?;
    let series: Vec<Value> = rows
        .into_iter()
        .map(|(date, rev, att, e, s, o, c)| json!({"date": date, "revenue": rev, "attributed_revenue": att, "emails_sent": e, "sms_sent": s, "opens": o, "clicks": c}))
        .collect();

    let top_campaigns: Vec<(Uuid, String, Option<chrono::DateTime<chrono::Utc>>)> = sqlx::query_as(&format!(
        "SELECT id, name, sent_at FROM campaigns WHERE account_id = $1 AND status IN ('sent','sending') AND sent_at >= {since} ORDER BY sent_at DESC LIMIT 10"
    ))
    .bind(account_id)
    .fetch_all(&state.db)
    .await?;
    let mut tc = Vec::new();
    for (id, name, sent_at) in top_campaigns {
        let s = crate::campaigns::stats(&state.db, account_id, id).await?;
        tc.push(json!({"id": id, "name": name, "sent_at": sent_at, "recipients": s.recipients, "open_rate": s.open_rate, "click_rate": s.click_rate, "revenue": s.revenue}));
    }
    tc.sort_by(|a, b| b["revenue"].as_f64().partial_cmp(&a["revenue"].as_f64()).unwrap_or(std::cmp::Ordering::Equal));

    let flows: Vec<(Uuid, String, String)> = sqlx::query_as("SELECT id, name, status FROM flows WHERE account_id = $1 AND status <> 'draft' ORDER BY updated_at DESC LIMIT 20")
        .bind(account_id)
        .fetch_all(&state.db)
        .await?;
    let mut tf = Vec::new();
    for (id, name, status) in flows {
        let (s, _, _) = crate::flows::stats(&state.db, account_id, id).await?;
        tf.push(json!({"id": id, "name": name, "status": status, "recipients": s.recipients, "revenue": s.revenue, "open_rate": s.open_rate, "click_rate": s.click_rate}));
    }
    tf.sort_by(|a, b| b["revenue"].as_f64().partial_cmp(&a["revenue"].as_f64()).unwrap_or(std::cmp::Ordering::Equal));

    let rate = |n: i64, d: i64| if d > 0 { n as f64 / d as f64 } else { 0.0 };
    Ok(json!({
        "days": days,
        "totals": {
            "revenue": revenue, "attributed_revenue": attributed, "campaign_revenue": campaign_rev, "flow_revenue": flow_rev,
            "emails_sent": sent_email, "sms_sent": sent_sms,
            "open_rate": rate(opens, delivered), "click_rate": rate(clicks, delivered + sent_sms),
            "unsubscribe_rate": rate(unsubs, sent_email + sent_sms), "bounce_rate": rate(bounces, sent_email),
            "profiles": profiles, "new_profiles": new_profiles,
        },
        "series": series,
        "top_campaigns": tc,
        "top_flows": tf,
    }))
}

async fn overview_h(State(state): State<AppState>, ctx: Ctx, Query(q): Query<Days>) -> AppResult<Json<Value>> {
    Ok(Json(overview(&state, ctx.account_id, q.days.unwrap_or(30)).await?))
}
