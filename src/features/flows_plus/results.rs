//! Per-step results on the flow canvas, the list of people waiting at a step, and taking people
//! out of a flow.
//!
//! [`results`] answers for every step of a flow in one query: people who reached it (runs that
//! started in the range and passed through the step), who is waiting there now, and for message
//! steps what happened to the messages created in the range, the main skip reasons, and the orders
//! and revenue the reports module's attribution credits to the step for conversions in the range.
//! It runs as a report (one at a time per account, a shared limit across the server, one deadline).

use chrono::{DateTime, NaiveDate, Utc};
use serde::Deserialize;
use serde_json::{json, Value};
use uuid::Uuid;

use crate::auth::Ctx;
use crate::error::{AppError, AppResult};
use crate::features::reports::query::{self, Mode, Run};
use crate::state::AppState;

/// The per-step results query. Binds: $1 account, $2 flow, $3 range start, $4 range end.
pub fn results_sql() -> String {
    format!(
        "WITH reached AS (
            SELECT v.node_id, count(*)::bigint AS n
            FROM flow_runs r CROSS JOIN LATERAL jsonb_array_elements_text(r.visited) v(node_id)
            WHERE r.flow_id = $2 AND r.account_id = $1 AND r.started_at >= $3 AND r.started_at < $4
            GROUP BY 1),
         waiting AS (
            SELECT r.current_node AS node_id, count(*)::bigint AS n FROM flow_runs r
            WHERE r.flow_id = $2 AND r.status = 'active' AND r.account_id = $1 AND r.current_node IS NOT NULL
            GROUP BY 1),
         msgs AS (
            SELECT m.flow_node_id AS node_id, m.channel,
                   count(*)::bigint AS recipients,
                   count(m.sent_at)::bigint AS sent,
                   count(*) FILTER (WHERE m.status IN ('sent','delivered') OR m.opened_at IS NOT NULL)::bigint AS delivered,
                   count(m.opened_at)::bigint AS opened,
                   count(m.clicked_at)::bigint AS clicked,
                   count(*) FILTER (WHERE m.status = 'bounced')::bigint AS bounced,
                   count(m.unsubscribed_at)::bigint AS unsubscribed,
                   count(*) FILTER (WHERE m.status = 'skipped')::bigint AS skipped,
                   count(*) FILTER (WHERE m.status = 'failed')::bigint AS failed
            FROM messages m
            WHERE m.flow_id = $2 AND m.account_id = $1 AND NOT m.is_test AND m.created_at >= $3 AND m.created_at < $4
            GROUP BY 1, 2),
         reasons AS (
            SELECT node_id, error, n FROM (
                SELECT m.flow_node_id AS node_id, COALESCE(m.error, 'unknown') AS error, count(*)::bigint AS n,
                       row_number() OVER (PARTITION BY m.flow_node_id ORDER BY count(*) DESC, COALESCE(m.error, 'unknown')) AS rk
                FROM messages m
                WHERE m.flow_id = $2 AND m.account_id = $1 AND NOT m.is_test AND m.status = 'skipped' AND m.created_at >= $3 AND m.created_at < $4
                GROUP BY 1, 2) t
            WHERE rk <= 3),
         credited AS (
            SELECT x.flow_node_id AS node_id, count(*)::bigint AS orders, COALESCE(sum(x.value), 0)::float8 AS revenue
            FROM ({att} AND e.occurred_at >= $3 AND e.occurred_at < $4
                  AND e.profile_id IN (SELECT profile_id FROM messages WHERE flow_id = $2)) x
            WHERE x.flow_id = $2
            GROUP BY 1),
         notes AS (
            SELECT n.node_id, count(*) FILTER (WHERE n.status = 'sent')::bigint AS sent,
                   count(*) FILTER (WHERE n.status = 'skipped')::bigint AS skipped,
                   count(*) FILTER (WHERE n.status = 'failed')::bigint AS failed,
                   count(*) FILTER (WHERE n.status = 'unconfirmed')::bigint AS unconfirmed
            FROM flow_notifications n
            WHERE n.flow_id = $2 AND n.account_id = $1 AND n.created_at >= $3 AND n.created_at < $4
            GROUP BY 1)
         SELECT
            (SELECT COALESCE(jsonb_object_agg(node_id, n), '{{}}') FROM reached),
            (SELECT COALESCE(jsonb_object_agg(node_id, n), '{{}}') FROM waiting),
            (SELECT COALESCE(jsonb_agg(to_jsonb(msgs)), '[]') FROM msgs),
            (SELECT COALESCE(jsonb_agg(jsonb_build_object('node_id', node_id, 'reason', error, 'count', n) ORDER BY node_id, n DESC), '[]') FROM reasons),
            (SELECT COALESCE(jsonb_object_agg(node_id, jsonb_build_object('orders', orders, 'revenue', revenue)), '{{}}') FROM credited WHERE node_id IS NOT NULL),
            (SELECT COALESCE(jsonb_agg(to_jsonb(notes)), '[]') FROM notes)",
        att = crate::features::reports::attribution::ATTRIBUTION_SQL
    )
}

#[derive(Deserialize, Default)]
pub struct Range {
    pub days: Option<i64>,
    pub start: Option<NaiveDate>,
    pub end: Option<NaiveDate>,
}

fn rate(n: i64, d: i64) -> f64 {
    if d > 0 { n as f64 / d as f64 } else { 0.0 }
}

pub async fn results(state: &AppState, account_id: Uuid, flow_id: Uuid, range: Range) -> AppResult<Value> {
    let flow = crate::api::messaging::load_flow(state, account_id, flow_id).await?;
    let def = crate::flows::parse_definition(&flow.definition)?;
    let (tz_name, tz) = query::account_tz(state, account_id).await?;
    let w = query::resolve_range(tz, range.days, range.start, range.end)?;
    let run = Run::start(account_id, Mode::Request).await?;
    let mut tx = run.tx(state).await?;
    let sql = results_sql();
    let row: (Value, Value, Value, Value, Value, Value) = sqlx::query_as(&sql)
        .bind(account_id)
        .bind(flow.id)
        .bind(w.start)
        .bind(w.end)
        .fetch_one(&mut **tx.arm().await?)
        .await
        .map_err(query::db_err)?;
    tx.commit().await?;
    let (reached, waiting, msgs, reasons, credited, notes) = row;
    let num = |v: &Value, id: &str| v.get(id).and_then(Value::as_i64).unwrap_or(0);
    let mut steps = Vec::new();
    for n in &def.nodes {
        let mut s = json!({"node_id": n.id, "type": n.kind, "reached": num(&reached, &n.id), "waiting": num(&waiting, &n.id)});
        if n.kind == "email" || n.kind == "sms" || msgs.as_array().is_some_and(|a| a.iter().any(|m| m["node_id"] == n.id.as_str())) {
            let m = msgs.as_array().and_then(|a| a.iter().find(|m| m["node_id"] == n.id.as_str())).cloned().unwrap_or(json!({}));
            let g = |k: &str| m.get(k).and_then(Value::as_i64).unwrap_or(0);
            let c = credited.get(&n.id).cloned().unwrap_or(json!({"orders": 0, "revenue": 0.0}));
            let top: Vec<Value> = reasons.as_array().into_iter().flatten().filter(|r| r["node_id"] == n.id.as_str()).map(|r| json!({"reason": r["reason"], "count": r["count"]})).collect();
            s["messages"] = json!({
                "recipients": g("recipients"), "sent": g("sent"), "delivered": g("delivered"), "opened": g("opened"), "clicked": g("clicked"),
                "bounced": g("bounced"), "unsubscribed": g("unsubscribed"), "skipped": g("skipped"), "failed": g("failed"),
                "open_rate": rate(g("opened"), g("delivered")), "click_rate": rate(g("clicked"), g("delivered")),
                "skip_reasons": top, "orders": c["orders"], "revenue": c["revenue"],
            });
        }
        if n.kind == "notify" {
            let x = notes.as_array().and_then(|a| a.iter().find(|m| m["node_id"] == n.id.as_str())).cloned().unwrap_or(json!({}));
            let g = |k: &str| x.get(k).and_then(Value::as_i64).unwrap_or(0);
            s["notifications"] = json!({"sent": g("sent"), "skipped": g("skipped"), "failed": g("failed"), "unconfirmed": g("unconfirmed")});
        }
        steps.push(s);
    }
    Ok(json!({
        "flow_id": flow.id, "range": {"start": w.start, "end": w.end, "timezone": tz_name},
        "note": "Reached counts runs that started in the range and passed the step; waiting is right now; message numbers cover messages created in the range; revenue is conversions in the range credited to the step.",
        "steps": steps,
    }))
}

pub const MAX_PAGE: i64 = 200;

#[derive(Deserialize, Default)]
pub struct WaitingQuery {
    pub limit: Option<i64>,
    pub after: Option<Uuid>,
}

/// People waiting at a step now, oldest run first, paged by run id.
pub async fn waiting(state: &AppState, account_id: Uuid, flow_id: Uuid, node_id: &str, q: WaitingQuery) -> AppResult<Value> {
    let flow = crate::api::messaging::load_flow(state, account_id, flow_id).await?;
    let limit = q.limit.unwrap_or(50).clamp(1, MAX_PAGE);
    type R = (Uuid, Uuid, DateTime<Utc>, Option<String>, Option<String>, Option<String>, Option<String>, Option<String>);
    let rows: Vec<R> = sqlx::query_as(
        "SELECT r.id, r.profile_id, r.started_at, r.context->>'_wait_until', p.email, p.phone_number, p.first_name, p.last_name
         FROM flow_runs r JOIN profiles p ON p.id = r.profile_id
         WHERE r.flow_id = $1 AND r.status = 'active' AND r.account_id = $2 AND r.current_node = $3 AND ($4::uuid IS NULL OR r.id > $4)
         ORDER BY r.id LIMIT $5",
    )
    .bind(flow.id)
    .bind(account_id)
    .bind(node_id)
    .bind(q.after)
    .bind(limit + 1)
    .fetch_all(&state.db)
    .await?;
    let more = rows.len() as i64 > limit;
    let data: Vec<Value> = rows
        .iter()
        .take(limit as usize)
        .map(|(run_id, pid, started, until, email, phone, first, last)| {
            json!({"run_id": run_id, "profile_id": pid, "entered_at": started, "waiting_until": until, "email": email, "phone_number": phone, "first_name": first, "last_name": last})
        })
        .collect();
    let next = if more { data.last().map(|d| d["run_id"].clone()) } else { None };
    Ok(json!({"data": data, "next_after": next}))
}

pub const MAX_REMOVE: usize = 500;

#[derive(Deserialize)]
pub struct RemoveBody {
    pub run_ids: Vec<Uuid>,
    #[serde(default)]
    pub confirm: bool,
}

/// Take people out of a flow (their runs end as exited; nothing more is sent to them by it).
/// Without `confirm`, says how many would be taken out.
pub async fn remove(state: &AppState, ctx: &Ctx, flow_id: Uuid, b: RemoveBody) -> AppResult<Value> {
    let flow = crate::api::messaging::load_flow(state, ctx.account_id, flow_id).await?;
    if b.run_ids.is_empty() || b.run_ids.len() > MAX_REMOVE {
        return Err(AppError::bad(format!("give 1-{MAX_REMOVE} run_ids")));
    }
    if !b.confirm {
        let n: i64 = sqlx::query_scalar("SELECT count(*) FROM flow_runs WHERE id = ANY($1) AND flow_id = $2 AND account_id = $3 AND status = 'active'")
            .bind(&b.run_ids)
            .bind(flow.id)
            .bind(ctx.account_id)
            .fetch_one(&state.db)
            .await?;
        return Ok(json!({"dry_run": true, "would_remove": n, "confirm": "Call again with confirm: true to take them out of the flow."}));
    }
    // A run whose current step is sending right now is left alone (the send claimed it first);
    // it's reported back so it can be removed once the step is done.
    let ids: Vec<Uuid> = sqlx::query_scalar(
        "UPDATE flow_runs SET status = 'exited', finished_at = now(), current_node = NULL
         WHERE id = ANY($1) AND flow_id = $2 AND account_id = $3 AND status = 'active' AND (sending_until IS NULL OR sending_until < now())
         RETURNING id",
    )
    .bind(&b.run_ids)
    .bind(flow.id)
    .bind(ctx.account_id)
    .fetch_all(&state.db)
    .await?;
    let busy: Vec<Uuid> = sqlx::query_scalar("SELECT id FROM flow_runs WHERE id = ANY($1) AND flow_id = $2 AND account_id = $3 AND status = 'active'")
        .bind(&b.run_ids)
        .bind(flow.id)
        .bind(ctx.account_id)
        .fetch_all(&state.db)
        .await?;
    crate::features::team::audit(state, ctx, "flow.remove_people", Some(&flow.id.to_string()), json!({"flow": flow.name, "removed": ids.len()})).await;
    Ok(json!({
        "removed": ids.len(), "run_ids": ids, "sending_now": busy,
        "note": if busy.is_empty() { "" } else { "Some people were being sent a message by this flow at that moment and weren't removed; try again in a minute." },
    }))
}
