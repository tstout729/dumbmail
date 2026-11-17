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
    Router::new()
        .route("/api/analytics/overview", get(overview_h))
        .route("/api/analytics/flows", get(flows_h))
        .route("/api/analytics/forms", get(forms_h))
}

#[derive(Deserialize)]
struct Days {
    days: Option<i64>,
}

/// Window totals for `[now - from_days, now - to_days)`: sends, engagement, revenue and orders.
async fn window_totals(state: &AppState, account_id: Uuid, from_days: i64, to_days: i64) -> AppResult<Value> {
    let now = chrono::Utc::now();
    let (lo, hi) = (now - chrono::Duration::days(from_days), now - chrono::Duration::days(to_days));

    let (sent_email, sent_sms, delivered, opens, clicks, email_clicks, unsubs, bounces, complaints): (i64, i64, i64, i64, i64, i64, i64, i64, i64) = sqlx::query_as(
        "SELECT count(*) FILTER (WHERE channel = 'email' AND status NOT IN ('queued','sending','failed','skipped')),
                count(*) FILTER (WHERE channel = 'sms' AND status NOT IN ('queued','sending','failed','skipped')),
                count(*) FILTER (WHERE channel = 'email' AND status IN ('sent','delivered')),
                count(opened_at) FILTER (WHERE channel = 'email'),
                count(clicked_at),
                count(clicked_at) FILTER (WHERE channel = 'email'),
                count(unsubscribed_at),
                count(*) FILTER (WHERE status = 'bounced'),
                count(*) FILTER (WHERE status = 'complained')
         FROM messages WHERE account_id = $1 AND NOT is_test AND created_at >= $2 AND created_at < $3"
    )
    .bind(account_id)
    .bind(lo)
    .bind(hi)
    .fetch_one(&state.db)
    .await?;

    let (revenue, attributed, campaign_rev, flow_rev, orders, attributed_orders): (f64, f64, f64, f64, i64, i64) = sqlx::query_as(&format!(
        "SELECT COALESCE(sum(value), 0)::float8,
                COALESCE(sum(value) FILTER (WHERE campaign_id IS NOT NULL OR flow_id IS NOT NULL), 0)::float8,
                COALESCE(sum(value) FILTER (WHERE campaign_id IS NOT NULL), 0)::float8,
                COALESCE(sum(value) FILTER (WHERE flow_id IS NOT NULL), 0)::float8,
                count(*),
                count(*) FILTER (WHERE campaign_id IS NOT NULL OR flow_id IS NOT NULL)
         FROM ({ATTRIBUTION_SQL} AND e.occurred_at >= $2 AND e.occurred_at < $3) x"
    ))
    .bind(account_id)
    .bind(lo)
    .bind(hi)
    .fetch_one(&state.db)
    .await?;

    let new_profiles: i64 = sqlx::query_scalar("SELECT count(*) FROM profiles WHERE account_id = $1 AND created_at >= $2 AND created_at < $3")
        .bind(account_id)
        .bind(lo)
        .bind(hi)
        .fetch_one(&state.db)
        .await?;

    let rate = |n: i64, d: i64| if d > 0 { n as f64 / d as f64 } else { 0.0 };
    Ok(json!({
        "revenue": revenue, "attributed_revenue": attributed, "campaign_revenue": campaign_rev, "flow_revenue": flow_rev,
        "orders": orders, "attributed_orders": attributed_orders,
        "emails_sent": sent_email, "sms_sent": sent_sms, "emails_delivered": delivered, "opens": opens, "clicks": clicks, "email_clicks": email_clicks,
        "open_rate": rate(opens, delivered), "click_rate": rate(clicks, delivered + sent_sms),
        "unsubscribe_rate": rate(unsubs, sent_email + sent_sms), "bounce_rate": rate(bounces, sent_email),
        "complaint_rate": rate(complaints, sent_email),
        "new_profiles": new_profiles,
    }))
}

/// Dashboard numbers for the last `days` days, bucketed by day in the account's time zone, with the
/// same totals for the `days` before that (`previous`) so the UI can show change.
pub async fn overview(state: &AppState, account_id: Uuid, days: i64) -> AppResult<Value> {
    let days = days.clamp(1, 365);
    let tz: String = sqlx::query_scalar("SELECT timezone FROM accounts WHERE id = $1").bind(account_id).fetch_one(&state.db).await?;
    let since = chrono::Utc::now() - chrono::Duration::days(days);

    let mut totals = window_totals(state, account_id, days, 0).await?;
    let previous = window_totals(state, account_id, days * 2, days).await?;
    let profiles: i64 = sqlx::query_scalar("SELECT count(*) FROM profiles WHERE account_id = $1").bind(account_id).fetch_one(&state.db).await?;
    totals["profiles"] = json!(profiles);

    let rows: Vec<(String, f64, f64, f64, f64, i64, i64, i64, i64, i64)> = sqlx::query_as(&format!(
        "WITH d AS (
            SELECT to_char(g, 'YYYY-MM-DD') AS day FROM generate_series(
              date_trunc('day', ($3) AT TIME ZONE $2), date_trunc('day', now() AT TIME ZONE $2), interval '1 day') g),
         m AS (
            SELECT to_char(created_at AT TIME ZONE $2, 'YYYY-MM-DD') AS day,
                   count(*) FILTER (WHERE channel = 'email' AND status NOT IN ('queued','sending','failed','skipped')) AS emails,
                   count(*) FILTER (WHERE channel = 'sms' AND status NOT IN ('queued','sending','failed','skipped')) AS sms
            FROM messages WHERE account_id = $1 AND NOT is_test AND created_at >= $3 GROUP BY 1),
         o AS (SELECT to_char(opened_at AT TIME ZONE $2, 'YYYY-MM-DD') AS day, count(*) AS n FROM messages WHERE account_id = $1 AND NOT is_test AND opened_at >= $3 GROUP BY 1),
         c AS (SELECT to_char(clicked_at AT TIME ZONE $2, 'YYYY-MM-DD') AS day, count(*) AS n FROM messages WHERE account_id = $1 AND NOT is_test AND clicked_at >= $3 GROUP BY 1),
         r AS (SELECT to_char(occurred_at AT TIME ZONE $2, 'YYYY-MM-DD') AS day, sum(value) AS rev,
                      sum(value) FILTER (WHERE campaign_id IS NOT NULL OR flow_id IS NOT NULL) AS att,
                      sum(value) FILTER (WHERE campaign_id IS NOT NULL) AS camp, sum(value) FILTER (WHERE flow_id IS NOT NULL) AS flow
               FROM ({ATTRIBUTION_SQL} AND e.occurred_at >= $3) x GROUP BY 1),
         p AS (SELECT to_char(created_at AT TIME ZONE $2, 'YYYY-MM-DD') AS day, count(*) AS n FROM profiles WHERE account_id = $1 AND created_at >= $3 GROUP BY 1)
         SELECT d.day, COALESCE(r.rev, 0)::float8, COALESCE(r.att, 0)::float8, COALESCE(r.camp, 0)::float8, COALESCE(r.flow, 0)::float8,
                COALESCE(m.emails, 0), COALESCE(m.sms, 0), COALESCE(o.n, 0), COALESCE(c.n, 0), COALESCE(p.n, 0)
         FROM d LEFT JOIN m USING (day) LEFT JOIN o USING (day) LEFT JOIN c USING (day) LEFT JOIN r USING (day) LEFT JOIN p USING (day) ORDER BY d.day"
    ))
    .bind(account_id)
    .bind(&tz)
    .bind(since)
    .fetch_all(&state.db)
    .await?;
    let series: Vec<Value> = rows
        .into_iter()
        .map(|(date, rev, att, camp, flow, e, s, o, c, p)| {
            json!({"date": date, "revenue": rev, "attributed_revenue": att, "campaign_revenue": camp, "flow_revenue": flow,
                   "emails_sent": e, "sms_sent": s, "opens": o, "clicks": c, "new_profiles": p})
        })
        .collect();

    let top_campaigns: Vec<(Uuid, String, Option<chrono::DateTime<chrono::Utc>>)> = sqlx::query_as(
        "SELECT id, name, sent_at FROM campaigns WHERE account_id = $1 AND status IN ('sent','sending','testing') AND sent_at >= $2 ORDER BY sent_at DESC LIMIT 10"
    )
    .bind(account_id)
    .bind(since)
    .fetch_all(&state.db)
    .await?;
    let mut tc = Vec::new();
    for (id, name, sent_at) in top_campaigns {
        let s = crate::campaigns::stats(&state.db, account_id, id).await?;
        tc.push(json!({"id": id, "name": name, "sent_at": sent_at, "recipients": s.recipients, "open_rate": s.open_rate, "click_rate": s.click_rate, "orders": s.orders, "revenue": s.revenue}));
    }
    tc.sort_by(|a, b| b["revenue"].as_f64().partial_cmp(&a["revenue"].as_f64()).unwrap_or(std::cmp::Ordering::Equal));

    // Flow numbers cover this window only, so they add up to the flow revenue in `totals`.
    let flows: Vec<(Uuid, String, String, i64, i64, i64, i64)> = sqlx::query_as(
        "SELECT f.id, f.name, f.status,
                count(m.id) FILTER (WHERE m.status NOT IN ('queued','sending','failed','skipped')),
                count(m.id) FILTER (WHERE m.status IN ('sent','delivered') OR m.opened_at IS NOT NULL),
                count(m.opened_at) FILTER (WHERE m.channel = 'email'),
                count(m.clicked_at)
         FROM flows f LEFT JOIN messages m ON m.flow_id = f.id AND NOT m.is_test AND m.created_at >= $2
         WHERE f.account_id = $1 AND f.status <> 'draft'
         GROUP BY f.id ORDER BY max(f.updated_at) DESC LIMIT 20"
    )
    .bind(account_id)
    .bind(since)
    .fetch_all(&state.db)
    .await?;
    let flow_rev: Vec<(Uuid, i64, f64)> = sqlx::query_as(&format!(
        "SELECT flow_id, count(*), COALESCE(sum(value), 0)::float8 FROM ({ATTRIBUTION_SQL} AND e.occurred_at >= $2) x WHERE flow_id IS NOT NULL GROUP BY 1"
    ))
    .bind(account_id)
    .bind(since)
    .fetch_all(&state.db)
    .await?;
    let rate = |n: i64, d: i64| if d > 0 { n as f64 / d as f64 } else { 0.0 };
    let mut tf = Vec::new();
    for (id, name, status, recipients, delivered, opens, clicks) in flows {
        let (orders, revenue) = flow_rev.iter().find(|r| r.0 == id).map(|r| (r.1, r.2)).unwrap_or((0, 0.0));
        tf.push(json!({"id": id, "name": name, "status": status, "recipients": recipients, "orders": orders, "revenue": revenue,
                       "open_rate": rate(opens, delivered), "click_rate": rate(clicks, delivered)}));
    }
    tf.sort_by(|a, b| b["revenue"].as_f64().partial_cmp(&a["revenue"].as_f64()).unwrap_or(std::cmp::Ordering::Equal));

    Ok(json!({
        "days": days,
        "totals": totals,
        "previous": previous,
        "series": series,
        "top_campaigns": tc,
        "top_flows": tf,
    }))
}

async fn overview_h(State(state): State<AppState>, ctx: Ctx, Query(q): Query<Days>) -> AppResult<Json<Value>> {
    Ok(Json(overview(&state, ctx.account_id, q.days.unwrap_or(30)).await?))
}

/// Flow-only totals for `[now - from_days, now - to_days)`: what flows sent and what they earned.
async fn flow_window(state: &AppState, account_id: Uuid, from_days: i64, to_days: i64) -> AppResult<Value> {
    let now = chrono::Utc::now();
    let (lo, hi) = (now - chrono::Duration::days(from_days), now - chrono::Duration::days(to_days));
    let (sent, recipients, delivered, email_delivered, opens, clicks): (i64, i64, i64, i64, i64, i64) = sqlx::query_as(
        "SELECT count(*) FILTER (WHERE status NOT IN ('queued','sending','failed','skipped')),
                count(DISTINCT profile_id) FILTER (WHERE status NOT IN ('queued','sending','failed','skipped')),
                count(*) FILTER (WHERE status IN ('sent','delivered') OR opened_at IS NOT NULL),
                count(*) FILTER (WHERE channel = 'email' AND (status IN ('sent','delivered') OR opened_at IS NOT NULL)),
                count(opened_at) FILTER (WHERE channel = 'email'),
                count(clicked_at)
         FROM messages WHERE account_id = $1 AND flow_id IS NOT NULL AND NOT is_test AND created_at >= $2 AND created_at < $3"
    )
    .bind(account_id)
    .bind(lo)
    .bind(hi)
    .fetch_one(&state.db)
    .await?;
    let (store_revenue, revenue, orders): (f64, f64, i64) = sqlx::query_as(&format!(
        "SELECT COALESCE(sum(value), 0)::float8, COALESCE(sum(value) FILTER (WHERE flow_id IS NOT NULL), 0)::float8,
                count(*) FILTER (WHERE flow_id IS NOT NULL)
         FROM ({ATTRIBUTION_SQL} AND e.occurred_at >= $2 AND e.occurred_at < $3) x"
    ))
    .bind(account_id)
    .bind(lo)
    .bind(hi)
    .fetch_one(&state.db)
    .await?;
    let rate = |n: f64, d: f64| if d > 0.0 { n / d } else { 0.0 };
    Ok(json!({
        "revenue": revenue, "orders": orders, "store_revenue": store_revenue,
        "messages_sent": sent, "recipients": recipients, "delivered": delivered, "opens": opens, "clicks": clicks,
        "open_rate": rate(opens as f64, email_delivered as f64), "click_rate": rate(clicks as f64, delivered as f64),
        "revenue_per_recipient": rate(revenue, recipients as f64),
    }))
}

/// Flow performance for the last `days` days, the `days` before that, and flow revenue and sends by day.
pub async fn flows(state: &AppState, account_id: Uuid, days: i64) -> AppResult<Value> {
    let days = days.clamp(1, 365);
    let tz: String = sqlx::query_scalar("SELECT timezone FROM accounts WHERE id = $1").bind(account_id).fetch_one(&state.db).await?;
    let since = chrono::Utc::now() - chrono::Duration::days(days);
    let totals = flow_window(state, account_id, days, 0).await?;
    let previous = flow_window(state, account_id, days * 2, days).await?;
    let rows: Vec<(String, f64, i64)> = sqlx::query_as(&format!(
        "WITH d AS (
            SELECT to_char(g, 'YYYY-MM-DD') AS day FROM generate_series(
              date_trunc('day', ($3) AT TIME ZONE $2), date_trunc('day', now() AT TIME ZONE $2), interval '1 day') g),
         m AS (SELECT to_char(created_at AT TIME ZONE $2, 'YYYY-MM-DD') AS day, count(*) AS n
               FROM messages WHERE account_id = $1 AND flow_id IS NOT NULL AND NOT is_test AND created_at >= $3
                 AND status NOT IN ('queued','sending','failed','skipped') GROUP BY 1),
         r AS (SELECT to_char(occurred_at AT TIME ZONE $2, 'YYYY-MM-DD') AS day, sum(value) AS rev
               FROM ({ATTRIBUTION_SQL} AND e.occurred_at >= $3) x WHERE flow_id IS NOT NULL GROUP BY 1)
         SELECT d.day, COALESCE(r.rev, 0)::float8, COALESCE(m.n, 0) FROM d LEFT JOIN m USING (day) LEFT JOIN r USING (day) ORDER BY d.day"
    ))
    .bind(account_id)
    .bind(&tz)
    .bind(since)
    .fetch_all(&state.db)
    .await?;
    let series: Vec<Value> = rows.into_iter().map(|(date, rev, n)| json!({"date": date, "revenue": rev, "messages_sent": n})).collect();
    Ok(json!({"days": days, "totals": totals, "previous": previous, "series": series}))
}

async fn flows_h(State(state): State<AppState>, ctx: Ctx, Query(q): Query<Days>) -> AppResult<Json<Value>> {
    Ok(Json(flows(&state, ctx.account_id, q.days.unwrap_or(30)).await?))
}

/// Form views and sign-ups for `[now - from_days, now - to_days)`.
async fn form_window(state: &AppState, account_id: Uuid, from_days: i64, to_days: i64) -> AppResult<Value> {
    let now = chrono::Utc::now();
    let (lo, hi) = (now - chrono::Duration::days(from_days), now - chrono::Duration::days(to_days));
    let (views, submits): (i64, i64) = sqlx::query_as(
        "SELECT COALESCE(sum(views), 0)::int8, COALESCE(sum(submits), 0)::int8 FROM form_stats_hourly WHERE account_id = $1 AND hour >= $2 AND hour < $3",
    )
    .bind(account_id)
    .bind(lo)
    .bind(hi)
    .fetch_one(&state.db)
    .await?;
    let rate = if views > 0 { submits as f64 / views as f64 } else { 0.0 };
    Ok(json!({"views": views, "submits": submits, "submit_rate": rate}))
}

/// Sign-up form performance for the last `days` days, the `days` before that, sign-ups and views by
/// day, and each form's numbers in the range.
pub async fn forms(state: &AppState, account_id: Uuid, days: i64) -> AppResult<Value> {
    let days = days.clamp(1, 365);
    let tz: String = sqlx::query_scalar("SELECT timezone FROM accounts WHERE id = $1").bind(account_id).fetch_one(&state.db).await?;
    let since = chrono::Utc::now() - chrono::Duration::days(days);
    let totals = form_window(state, account_id, days, 0).await?;
    let previous = form_window(state, account_id, days * 2, days).await?;
    let rows: Vec<(String, i64, i64)> = sqlx::query_as(
        "WITH d AS (
            SELECT to_char(g, 'YYYY-MM-DD') AS day FROM generate_series(
              date_trunc('day', $3 AT TIME ZONE $2), date_trunc('day', now() AT TIME ZONE $2), interval '1 day') g),
         s AS (SELECT to_char(hour AT TIME ZONE $2, 'YYYY-MM-DD') AS day, sum(views) AS views, sum(submits) AS submits
               FROM form_stats_hourly WHERE account_id = $1 AND hour >= $3 GROUP BY 1)
         SELECT d.day, COALESCE(s.views, 0)::int8, COALESCE(s.submits, 0)::int8 FROM d LEFT JOIN s USING (day) ORDER BY d.day",
    )
    .bind(account_id)
    .bind(&tz)
    .bind(since)
    .fetch_all(&state.db)
    .await?;
    let series: Vec<Value> = rows.into_iter().map(|(date, v, s)| json!({"date": date, "views": v, "submits": s})).collect();
    let per_form: Vec<(Uuid, String, i64, i64)> = sqlx::query_as(
        "SELECT f.id, f.name, sum(h.views)::int8, sum(h.submits)::int8
         FROM form_stats_hourly h JOIN forms f ON f.id = h.form_id
         WHERE h.account_id = $1 AND h.hour >= $2 GROUP BY f.id ORDER BY 4 DESC, 3 DESC",
    )
    .bind(account_id)
    .bind(since)
    .fetch_all(&state.db)
    .await?;
    let forms: Vec<Value> = per_form
        .into_iter()
        .map(|(id, name, v, s)| json!({"id": id, "name": name, "views": v, "submits": s, "submit_rate": if v > 0 { s as f64 / v as f64 } else { 0.0 }}))
        .collect();
    Ok(json!({"days": days, "totals": totals, "previous": previous, "series": series, "forms": forms}))
}

async fn forms_h(State(state): State<AppState>, ctx: Ctx, Query(q): Query<Days>) -> AppResult<Json<Value>> {
    Ok(Json(forms(&state, ctx.account_id, q.days.unwrap_or(30)).await?))
}
