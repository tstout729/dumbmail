//! The audit log: who did what and when.
//!
//! It records what happened, not what was asked: each change is recorded by the code that makes
//! it, right after it's committed (a send started, a member removed), with a small fixed set of
//! fields for that action. REST handlers and MCP tools record the same actions. Request bodies and
//! MCP arguments are never copied in, dry runs aren't recorded, and [`scrub`] drops anything that
//! looks like a secret or message content anyway.

use axum::extract::{Query, State};
use axum::http::header;
use axum::response::{IntoResponse, Response};
use axum::Json;
use chrono::{DateTime, NaiveDate, TimeZone, Utc};
use serde::Deserialize;
use serde_json::{json, Map, Value};
use uuid::Uuid;

use crate::auth::Ctx;
use crate::error::{AppError, AppResult};
use crate::state::AppState;

/// How long entries are kept.
pub const RETENTION_MONTHS: i32 = 13;

/// Who did it, for entries written outside a request (sign-in attempts, system work).
#[derive(Debug, Clone, Default)]
pub struct Actor {
    pub user_id: Option<Uuid>,
    pub key_id: Option<Uuid>,
    pub ip: Option<String>,
    pub user_agent: Option<String>,
}

/// Record an action in the account's audit log. `target` names what it was done to (an id, an
/// email, a domain); `details` is a small JSON object. Never pass secrets or message content; keys
/// that look like them are dropped anyway. Failures are logged, not returned.
pub async fn audit(state: &AppState, ctx: &Ctx, action: &str, target: Option<&str>, details: Value) {
    let actor = Actor { user_id: ctx.user_id, key_id: ctx.api_key_id, ip: ctx.ip.clone(), user_agent: ctx.user_agent.clone() };
    record(state, ctx.account_id, &actor, action, target, details).await;
}

/// [`audit`] for code without a `Ctx`.
pub async fn record(state: &AppState, account_id: Uuid, actor: &Actor, action: &str, target: Option<&str>, details: Value) {
    let actor_type = if actor.key_id.is_some() {
        "api_key"
    } else if actor.user_id.is_some() {
        "user"
    } else {
        "system"
    };
    let details = scrub(details);
    let res = sqlx::query(
        "INSERT INTO audit_log (id, account_id, actor_type, actor_user_id, actor_key_id, actor_label, action, target, details, ip, user_agent)
         VALUES ($1, $2, $3, $4, $5,
            COALESCE((SELECT name || ' (' || prefix || '…)' FROM api_keys WHERE id = $5), (SELECT email FROM users WHERE id = $4)),
            $6, $7, $8, $9, $10)",
    )
    .bind(Uuid::now_v7())
    .bind(account_id)
    .bind(actor_type)
    .bind(actor.user_id)
    .bind(actor.key_id)
    .bind(truncate(action, 80))
    .bind(target.map(|t| truncate(t, 300)))
    .bind(details)
    .bind(actor.ip.as_deref().map(|s| truncate(s, 64)))
    .bind(actor.user_agent.as_deref().map(|s| truncate(s, 300)))
    .execute(&state.db)
    .await;
    if let Err(e) = res {
        tracing::error!(error = %e, action, "couldn't write audit log entry");
    }
}

fn truncate(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
}

const SECRET_WORDS: [&str; 12] = ["password", "secret", "token", "key", "code", "html", "body", "content", "text", "credential", "authorization", "cookie"];
const SAFE_KEYS: [&str; 6] = ["key_id", "key_name", "key_prefix", "api_key_id", "flow_key", "blueprint_key"];

/// Drop values whose names look like secrets or message content, and bound sizes.
pub fn scrub(v: Value) -> Value {
    fn walk(v: Value, depth: usize) -> Value {
        match v {
            Value::Object(m) if depth < 4 => {
                let mut out = Map::new();
                for (k, v) in m.into_iter().take(40) {
                    let lower = k.to_ascii_lowercase();
                    if !SAFE_KEYS.contains(&lower.as_str()) && SECRET_WORDS.iter().any(|w| lower.contains(w)) {
                        continue;
                    }
                    out.insert(truncate(&k, 60), walk(v, depth + 1));
                }
                Value::Object(out)
            }
            Value::Array(a) if depth < 4 => Value::Array(a.into_iter().take(50).map(|x| walk(x, depth + 1)).collect()),
            Value::String(s) => Value::String(truncate(&s, 500)),
            Value::Object(_) | Value::Array(_) => Value::String("…".into()),
            other => other,
        }
    }
    match walk(v, 0) {
        Value::Object(m) => Value::Object(m),
        Value::Null => json!({}),
        other => json!({ "value": other }),
    }
}

// ---- Typed entries for campaigns and flows ----

/// A campaign change: its id as the target, and its name, channel, status and send time.
pub async fn campaign(state: &AppState, ctx: &Ctx, action: &str, c: &crate::models::CampaignRow) {
    audit(state, ctx, action, Some(&c.id.to_string()), json!({"name": c.name, "channel": c.channel, "status": c.status, "send_at": c.send_at})).await;
}

/// A flow change: its id as the target, and its name and status.
pub async fn flow(state: &AppState, ctx: &Ctx, action: &str, f: &crate::models::FlowRow) {
    audit(state, ctx, action, Some(&f.id.to_string()), json!({"name": f.name, "status": f.status})).await;
}

// ---- Reading ----

#[derive(Debug, Default, Deserialize)]
pub struct AuditQuery {
    /// An action (`campaign.send`) or a prefix ending in a dot (`team.`).
    pub action: Option<String>,
    /// A member's user id.
    pub user_id: Option<Uuid>,
    /// An API key id.
    pub key_id: Option<Uuid>,
    /// Words to find in the actor, action or target.
    pub q: Option<String>,
    /// First day (YYYY-MM-DD) in the account's time zone.
    pub from: Option<String>,
    /// Last day (YYYY-MM-DD), included.
    pub to: Option<String>,
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

fn day_start(tz: chrono_tz::Tz, day: &str) -> AppResult<DateTime<Utc>> {
    let d = NaiveDate::parse_from_str(day.trim(), "%Y-%m-%d").map_err(|_| AppError::bad("dates look like 2026-11-17"))?;
    let local = d.and_hms_opt(0, 0, 0).expect("midnight");
    Ok(tz.from_local_datetime(&local).earliest().unwrap_or_else(|| tz.from_utc_datetime(&local)).with_timezone(&Utc))
}

fn next_day_start(tz: chrono_tz::Tz, day: &str) -> AppResult<DateTime<Utc>> {
    let d = NaiveDate::parse_from_str(day.trim(), "%Y-%m-%d").map_err(|_| AppError::bad("dates look like 2026-11-17"))?;
    let next = d.succ_opt().ok_or_else(|| AppError::bad("that date is out of range"))?;
    day_start(tz, &next.format("%Y-%m-%d").to_string())
}

async fn account_tz(state: &AppState, account_id: Uuid) -> AppResult<chrono_tz::Tz> {
    let tz: String = sqlx::query_scalar("SELECT timezone FROM accounts WHERE id = $1").bind(account_id).fetch_one(&state.db).await?;
    Ok(tz.parse().unwrap_or(chrono_tz::America::Los_Angeles))
}

#[derive(sqlx::FromRow)]
struct Row {
    id: Uuid,
    created_at: DateTime<Utc>,
    actor_type: String,
    actor_user_id: Option<Uuid>,
    actor_key_id: Option<Uuid>,
    actor_label: Option<String>,
    action: String,
    target: Option<String>,
    details: Value,
    ip: Option<String>,
    user_agent: Option<String>,
}

fn row_json(r: &Row, tz: chrono_tz::Tz) -> Value {
    json!({
        "id": r.id, "created_at": r.created_at, "local_time": r.created_at.with_timezone(&tz).format("%Y-%m-%d %H:%M:%S %Z").to_string(),
        "actor": {"type": r.actor_type, "user_id": r.actor_user_id, "key_id": r.actor_key_id, "label": r.actor_label},
        "action": r.action, "target": r.target, "details": r.details, "ip": r.ip, "user_agent": r.user_agent,
    })
}

async fn query_rows(state: &AppState, account_id: Uuid, q: &AuditQuery, limit: i64, offset: i64) -> AppResult<(Vec<Row>, i64, chrono_tz::Tz)> {
    let tz = account_tz(state, account_id).await?;
    let from = q.from.as_deref().filter(|s| !s.trim().is_empty()).map(|d| day_start(tz, d)).transpose()?;
    // The end is the next local midnight, not 24 hours later (days are 23 or 25 hours at DST changes).
    let to = q.to.as_deref().filter(|s| !s.trim().is_empty()).map(|d| next_day_start(tz, d)).transpose()?;
    let mut c = sqlx::QueryBuilder::new("SELECT count(*) FROM audit_log WHERE account_id = ");
    c.push_bind(account_id);
    push_filters(&mut c, q, from, to);
    let total: i64 = c.build_query_scalar().fetch_one(&state.db).await?;
    let mut sel = sqlx::QueryBuilder::new("SELECT id, created_at, actor_type, actor_user_id, actor_key_id, actor_label, action, target, details, ip, user_agent FROM audit_log WHERE account_id = ");
    sel.push_bind(account_id);
    push_filters(&mut sel, q, from, to);
    sel.push(" ORDER BY created_at DESC, id DESC LIMIT ").push_bind(limit).push(" OFFSET ").push_bind(offset);
    let rows: Vec<Row> = sel.build_query_as().fetch_all(&state.db).await?;
    Ok((rows, total, tz))
}

fn push_filters(qb: &mut sqlx::QueryBuilder<'_, sqlx::Postgres>, q: &AuditQuery, from: Option<DateTime<Utc>>, to: Option<DateTime<Utc>>) {
    if let Some(a) = q.action.as_deref().map(str::trim).filter(|a| !a.is_empty()) {
        let a = truncate(a, 80);
        if let Some(prefix) = a.strip_suffix('*').map(str::to_string).or(if a.ends_with('.') { Some(a.clone()) } else { None }) {
            qb.push(" AND starts_with(action, ").push_bind(prefix).push(")");
        } else {
            qb.push(" AND action = ").push_bind(a);
        }
    }
    if let Some(u) = q.user_id {
        qb.push(" AND actor_user_id = ").push_bind(u);
    }
    if let Some(k) = q.key_id {
        qb.push(" AND actor_key_id = ").push_bind(k);
    }
    if let Some(words) = q.q.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        let pat = format!("%{}%", truncate(words, 100).replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_"));
        qb.push(" AND (actor_label ILIKE ").push_bind(pat.clone()).push(" OR action ILIKE ").push_bind(pat.clone()).push(" OR target ILIKE ").push_bind(pat).push(")");
    }
    if let Some(f) = from {
        qb.push(" AND created_at >= ").push_bind(f);
    }
    if let Some(t) = to {
        qb.push(" AND created_at < ").push_bind(t);
    }
}

/// A page of the audit log, newest first.
pub async fn list(state: &AppState, account_id: Uuid, q: &AuditQuery) -> AppResult<Value> {
    let limit = q.limit.unwrap_or(50).clamp(1, 200);
    let offset = q.offset.unwrap_or(0).clamp(0, 100_000);
    let (rows, total, tz) = query_rows(state, account_id, q, limit, offset).await?;
    Ok(json!({"data": rows.iter().map(|r| row_json(r, tz)).collect::<Vec<_>>(), "total": total, "timezone": tz.name()}))
}

/// Most rows one CSV export holds.
pub const EXPORT_MAX: i64 = 50_000;

pub async fn export_csv(state: &AppState, account_id: Uuid, q: &AuditQuery) -> AppResult<(String, i64)> {
    let (rows, total, tz) = query_rows(state, account_id, q, EXPORT_MAX, 0).await?;
    let mut w = csv::Writer::from_writer(Vec::new());
    let err = |e: csv::Error| AppError::Internal(e.into());
    w.write_record(["time", "actor", "actor_type", "action", "target", "details", "ip", "user_agent"]).map_err(err)?;
    for r in &rows {
        let time = r.created_at.with_timezone(&tz).format("%Y-%m-%d %H:%M:%S %Z").to_string();
        // Cells starting with these could run as formulas in a spreadsheet.
        let cell = |s: &str| if s.starts_with(['=', '+', '-', '@', '\t', '\r']) { format!("'{s}") } else { s.to_string() };
        w.write_record([
            time,
            cell(r.actor_label.as_deref().unwrap_or("")),
            r.actor_type.clone(),
            cell(&r.action),
            cell(r.target.as_deref().unwrap_or("")),
            cell(&r.details.to_string()),
            cell(r.ip.as_deref().unwrap_or("")),
            cell(r.user_agent.as_deref().unwrap_or("")),
        ])
        .map_err(err)?;
    }
    let bytes = w.into_inner().map_err(|e| AppError::Internal(anyhow::anyhow!("csv: {e}")))?;
    Ok((String::from_utf8_lossy(&bytes).into_owned(), total.min(EXPORT_MAX)))
}

pub async fn list_h(State(state): State<AppState>, ctx: Ctx, Query(q): Query<AuditQuery>) -> AppResult<Json<Value>> {
    Ok(Json(list(&state, ctx.account_id, &q).await?))
}

pub async fn export_h(State(state): State<AppState>, ctx: Ctx, Query(q): Query<AuditQuery>) -> AppResult<Response> {
    let (csv, rows) = export_csv(&state, ctx.account_id, &q).await?;
    audit(&state, &ctx, "audit.export", None, json!({"rows": rows})).await;
    let day = Utc::now().with_timezone(&account_tz(&state, ctx.account_id).await?).format("%Y-%m-%d");
    Ok((
        [(header::CONTENT_TYPE, "text/csv; charset=utf-8".to_string()), (header::CONTENT_DISPOSITION, format!("attachment; filename=\"audit-log-{day}.csv\""))],
        csv,
    )
        .into_response())
}

/// Drop entries older than the retention period, a batch at a time.
pub async fn prune(state: &AppState) -> anyhow::Result<()> {
    sqlx::query(
        "DELETE FROM audit_log WHERE id IN (SELECT id FROM audit_log WHERE created_at < now() - make_interval(months => $1) LIMIT 10000)",
    )
    .bind(RETENTION_MONTHS)
    .execute(&state.db)
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn day_ends_follow_dst() {
        let la = chrono_tz::America::Los_Angeles;
        // 2026-11-01 is 25 hours long in Los Angeles; 2026-03-08 is 23.
        let (a, b) = (day_start(la, "2026-11-01").unwrap(), next_day_start(la, "2026-11-01").unwrap());
        assert_eq!((b - a).num_hours(), 25);
        let (a, b) = (day_start(la, "2026-03-08").unwrap(), next_day_start(la, "2026-03-08").unwrap());
        assert_eq!((b - a).num_hours(), 23);
        assert_eq!(next_day_start(la, "2026-11-17").unwrap().to_rfc3339(), "2026-11-18T08:00:00+00:00");
    }

    #[test]
    fn scrub_drops_secrets() {
        let v = scrub(json!({"role": "admin", "password": "x", "client_secret": "y", "api_token": "z", "html": "<p>", "key_id": "k1", "nested": {"secret": 1, "ok": 2}}));
        assert_eq!(v, json!({"role": "admin", "key_id": "k1", "nested": {"ok": 2}}));
        assert_eq!(scrub(Value::Null), json!({}));
    }
}
