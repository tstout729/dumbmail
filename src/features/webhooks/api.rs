//! REST API: webhook endpoints, their delivery log, resends and tests, the flow signing secret, and
//! REST hooks (subscribe / unsubscribe / sample) for Zapier and Make.

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::{json, Map, Value};
use uuid::Uuid;

use super::deliver::{self, Endpoint};
use super::{net, topics};
use crate::api::ApiJson;
use crate::auth::Ctx;
use crate::error::{AppError, AppResult};
use crate::features::team::access::{rule, Perm, RouteRule};
use crate::features::team::audit;
use crate::state::AppState;

/// Who may use each route. Seeing endpoints and their log (no payloads) is for Managers and up;
/// changing them, secrets, tests, payloads, resends and REST hooks are for Admins and owners.
/// REST hooks are also made only with an API key (checked in the handlers).
pub static ROUTES: &[RouteRule] = &[
    rule("GET", "/api/webhooks", Perm::WebhooksRead),
    rule("POST", "/api/webhooks", Perm::WebhooksManage),
    rule("GET", "/api/webhooks/topics", Perm::WebhooksRead),
    rule("GET", "/api/webhooks/flow-secret", Perm::WebhooksRead),
    rule("POST", "/api/webhooks/flow-secret/rotate", Perm::WebhooksManage),
    rule("GET", "/api/webhooks/{id}", Perm::WebhooksRead),
    rule("PATCH", "/api/webhooks/{id}", Perm::WebhooksManage),
    rule("DELETE", "/api/webhooks/{id}", Perm::WebhooksManage),
    rule("POST", "/api/webhooks/{id}/rotate-secret", Perm::WebhooksManage),
    rule("POST", "/api/webhooks/{id}/test", Perm::WebhooksManage),
    rule("GET", "/api/webhooks/{id}/deliveries", Perm::WebhooksRead),
    rule("GET", "/api/webhooks/{id}/deliveries/{delivery_id}", Perm::WebhooksManage),
    rule("POST", "/api/webhooks/{id}/resend", Perm::WebhooksManage),
    rule("GET", "/api/hooks", Perm::WebhooksManage),
    rule("POST", "/api/hooks", Perm::WebhooksManage),
    rule("GET", "/api/hooks/me", Perm::WebhooksManage),
    rule("GET", "/api/hooks/samples", Perm::WebhooksManage),
    rule("DELETE", "/api/hooks/{id}", Perm::WebhooksManage),
];

const MAX_ENDPOINTS: i64 = 100;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/webhooks", get(list_h).post(create_h))
        .route("/api/webhooks/topics", get(topics_h))
        .route("/api/webhooks/flow-secret", get(flow_secret_h))
        .route("/api/webhooks/flow-secret/rotate", post(rotate_flow_secret_h))
        .route("/api/webhooks/{id}", get(get_h).patch(patch_h).delete(delete_h))
        .route("/api/webhooks/{id}/rotate-secret", post(rotate_h))
        .route("/api/webhooks/{id}/test", post(test_h))
        .route("/api/webhooks/{id}/deliveries", get(deliveries_h))
        .route("/api/webhooks/{id}/deliveries/{delivery_id}", get(delivery_h))
        .route("/api/webhooks/{id}/resend", post(resend_h))
        .route("/api/hooks", get(hooks_h).post(subscribe_h))
        .route("/api/hooks/me", get(me_h))
        .route("/api/hooks/samples", get(samples_h))
        .route("/api/hooks/{id}", axum::routing::delete(unsubscribe_h))
}

// ---------- Endpoint rows ----------

const LIST_COLS: &str = "e.id, e.url, e.description, e.topics, e.metrics, e.filter, e.trusted_only, e.enabled, e.source, e.batch_size, e.rate_per_second, e.secret_hint,
    e.previous_secret_until, e.consecutive_failures, e.failing_since, e.retry_at, e.disabled_reason, e.last_success_at, e.last_failure_at, e.created_at, e.updated_at,
    e.revision, e.dropped_total, e.last_dropped_at, e.pending::bigint AS pending,
    (SELECT count(*) FROM webhook_deliveries d WHERE d.endpoint_id = e.id AND d.status = 'delivered' AND d.created_at > now() - interval '24 hours') AS delivered_24h,
    (SELECT count(*) FROM webhook_deliveries d WHERE d.endpoint_id = e.id AND d.status = 'failed' AND d.created_at > now() - interval '24 hours') AS failed_24h";

#[derive(sqlx::FromRow)]
struct Row {
    id: Uuid,
    url: String,
    description: String,
    topics: Vec<String>,
    metrics: Vec<String>,
    filter: Option<Value>,
    trusted_only: bool,
    enabled: bool,
    source: String,
    batch_size: i32,
    rate_per_second: i32,
    secret_hint: String,
    previous_secret_until: Option<DateTime<Utc>>,
    consecutive_failures: i32,
    failing_since: Option<DateTime<Utc>>,
    retry_at: Option<DateTime<Utc>>,
    disabled_reason: Option<String>,
    last_success_at: Option<DateTime<Utc>>,
    last_failure_at: Option<DateTime<Utc>>,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
    revision: i32,
    dropped_total: i64,
    last_dropped_at: Option<DateTime<Utc>>,
    pending: i64,
    delivered_24h: i64,
    failed_24h: i64,
}

fn row_json(r: &Row) -> Value {
    let status = if !r.enabled {
        "disabled"
    } else if r.failing_since.is_some() {
        "failing"
    } else {
        "active"
    };
    json!({
        "id": r.id, "url": r.url, "description": r.description, "topics": r.topics, "metrics": r.metrics, "filter": r.filter, "trusted_only": r.trusted_only,
        "enabled": r.enabled, "status": status, "source": r.source, "batch_size": r.batch_size, "rate_per_second": r.rate_per_second,
        "secret_hint": r.secret_hint, "previous_secret_valid_until": r.previous_secret_until.filter(|t| *t > Utc::now()),
        "consecutive_failures": r.consecutive_failures, "failing_since": r.failing_since, "next_retry_at": r.retry_at.filter(|t| *t > Utc::now()),
        "disabled_reason": r.disabled_reason, "last_success_at": r.last_success_at, "last_failure_at": r.last_failure_at,
        "created_at": r.created_at, "updated_at": r.updated_at, "revision": r.revision,
        "stats": {"pending": r.pending, "delivered_24h": r.delivered_24h, "failed_24h": r.failed_24h, "dropped_total": r.dropped_total, "last_dropped_at": r.last_dropped_at},
    })
}

/// Who is asking. A REST hook (made through `/api/hooks`) belongs to its API key: only that key,
/// or a signed-in member who may manage webhooks, can see or change it. Other API keys and other
/// members don't see it at all, on any route or MCP tool.
#[derive(Clone, Copy)]
pub(super) struct Who {
    pub account_id: Uuid,
    key: Option<Uuid>,
    manager: bool,
}

impl Who {
    pub fn of(ctx: &Ctx) -> Who {
        Who { account_id: ctx.account_id, key: if ctx.via_api_key { ctx.api_key_id } else { None }, manager: !ctx.via_api_key && ctx.can(Perm::WebhooksManage) }
    }

    fn key(account_id: Uuid, key: Uuid) -> Who {
        Who { account_id, key: Some(key), manager: false }
    }
}

/// The endpoint `e` is visible to `$2` (the API key, or NULL) / `$3` (a member who may manage).
const VISIBLE: &str = "e.account_id = $1 AND (e.api_key_id IS NULL OR e.api_key_id = $2 OR $3)";

pub(super) async fn list(state: &AppState, who: Who) -> AppResult<Vec<Value>> {
    let rows: Vec<Row> = sqlx::query_as(&format!("SELECT {LIST_COLS} FROM webhook_endpoints e WHERE {VISIBLE} ORDER BY e.created_at"))
        .bind(who.account_id)
        .bind(who.key)
        .bind(who.manager)
        .fetch_all(&state.db)
        .await?;
    Ok(rows.iter().map(row_json).collect())
}

pub(super) async fn get_one(state: &AppState, who: Who, id: Uuid) -> AppResult<Value> {
    let row: Option<Row> = sqlx::query_as(&format!("SELECT {LIST_COLS} FROM webhook_endpoints e WHERE {VISIBLE} AND e.id = $4"))
        .bind(who.account_id)
        .bind(who.key)
        .bind(who.manager)
        .bind(id)
        .fetch_optional(&state.db)
        .await?;
    row.as_ref().map(row_json).ok_or_else(|| AppError::not_found("webhook endpoint"))
}

async fn endpoint(state: &AppState, who: Who, id: Uuid) -> AppResult<Endpoint> {
    let cols = deliver::ENDPOINT_COLS.split(", ").map(|c| format!("e.{}", c.trim())).collect::<Vec<_>>().join(", ");
    sqlx::query_as::<_, Endpoint>(&format!("SELECT {cols} FROM webhook_endpoints e WHERE {VISIBLE} AND e.id = $4"))
        .bind(who.account_id)
        .bind(who.key)
        .bind(who.manager)
        .bind(id)
        .fetch_optional(&state.db)
        .await?
        .ok_or_else(|| AppError::not_found("webhook endpoint"))
}

/// The host of a URL, for the audit log (never the full URL: it can be a credential).
fn host(url: &str) -> String {
    url::Url::parse(url).ok().and_then(|u| u.host_str().map(String::from)).unwrap_or_default()
}

// ---------- Validation ----------

/// The fields a create or update may set. `None` means "not given".
#[derive(Default)]
struct Fields {
    url: Option<String>,
    description: Option<String>,
    topics: Option<Vec<String>>,
    metrics: Option<Vec<String>>,
    filter: Option<Option<Value>>,
    trusted_only: Option<bool>,
    enabled: Option<bool>,
    batch_size: Option<i32>,
    rate_per_second: Option<i32>,
}

async fn parse_fields(b: &Value) -> AppResult<Fields> {
    let obj = b.as_object().ok_or_else(|| AppError::bad("send a JSON object"))?;
    let allowed = ["url", "description", "topics", "metrics", "filter", "trusted_only", "enabled", "batch_size", "rate_per_second", "revision"];
    if let Some(k) = obj.keys().find(|k| !allowed.contains(&k.as_str())) {
        return Err(AppError::bad(format!("unknown field {k:?}; allowed: {}", allowed.join(", "))));
    }
    let mut f = Fields::default();
    if let Some(v) = obj.get("url") {
        let url = v.as_str().ok_or_else(|| AppError::bad("url must be a string"))?.trim().to_string();
        net::check(&url, "webhook url").await?;
        f.url = Some(url);
    }
    if let Some(v) = obj.get("description") {
        let d = v.as_str().unwrap_or("").trim().to_string();
        if d.chars().count() > 500 {
            return Err(AppError::bad("description is 500 characters at most"));
        }
        f.description = Some(d);
    }
    if let Some(v) = obj.get("topics") {
        let list: Vec<String> = serde_json::from_value(v.clone()).map_err(|_| AppError::bad("topics must be a list of topic names"))?;
        let mut out: Vec<String> = Vec::new();
        for t in list {
            if !topics::is_topic(&t) {
                return Err(AppError::bad(format!("unknown topic {t:?} (see GET /api/webhooks/topics)")));
            }
            if !out.contains(&t) {
                out.push(t);
            }
        }
        if out.is_empty() {
            return Err(AppError::bad("pick at least one topic"));
        }
        f.topics = Some(out);
    }
    if let Some(v) = obj.get("metrics") {
        let list: Vec<String> = if v.is_null() { Vec::new() } else { serde_json::from_value(v.clone()).map_err(|_| AppError::bad("metrics must be a list of metric names"))? };
        if list.len() > 50 || list.iter().any(|m| m.trim().is_empty() || m.len() > 128) {
            return Err(AppError::bad("metrics: at most 50 names of 1-128 characters"));
        }
        f.metrics = Some(list.into_iter().map(|m| m.trim().to_string()).collect());
    }
    if let Some(v) = obj.get("filter") {
        if v.is_null() || v.as_array().is_some_and(|a| a.is_empty()) {
            f.filter = Some(None);
        } else {
            let conds: Vec<crate::segments::Filter> =
                serde_json::from_value(v.clone()).map_err(|e| AppError::bad(format!("filter must be a list of {{field, op, value}} conditions: {e}")))?;
            if conds.len() > 20 || conds.iter().any(|c| c.field.is_empty() || c.field.len() > 200) {
                return Err(AppError::bad("filter: at most 20 conditions, each with a field of up to 200 characters"));
            }
            f.filter = Some(Some(v.clone()));
        }
    }
    if let Some(v) = obj.get("enabled") {
        f.enabled = Some(v.as_bool().ok_or_else(|| AppError::bad("enabled must be true or false"))?);
    }
    if let Some(v) = obj.get("trusted_only") {
        f.trusted_only = Some(v.as_bool().ok_or_else(|| AppError::bad("trusted_only must be true or false"))?);
    }
    let int = |k: &str, lo: i64, hi: i64| -> AppResult<Option<i32>> {
        match obj.get(k) {
            None => Ok(None),
            Some(v) => match v.as_i64() {
                Some(n) if (lo..=hi).contains(&n) => Ok(Some(n as i32)),
                _ => Err(AppError::bad(format!("{k} must be a whole number from {lo} to {hi}"))),
            },
        }
    };
    f.batch_size = int("batch_size", 1, 100)?;
    f.rate_per_second = int("rate_per_second", 1, 100)?;
    Ok(f)
}

// ---------- Actions (shared by the REST API and MCP) ----------

pub(super) async fn create(state: &AppState, ctx: &Ctx, body: &Value, source: &str, default_batch: i32, api_key_id: Option<Uuid>) -> AppResult<Value> {
    let account_id = ctx.account_id;
    let f = parse_fields(body).await?;
    let url = f.url.ok_or_else(|| AppError::bad("url is required"))?;
    let topics = f.topics.ok_or_else(|| AppError::bad("topics is required"))?;
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM webhook_endpoints WHERE account_id = $1").bind(account_id).fetch_one(&state.db).await?;
    if n >= MAX_ENDPOINTS {
        return Err(AppError::Conflict(format!("an account can have {MAX_ENDPOINTS} webhook endpoints; delete one first")));
    }
    let secret = super::new_secret();
    let id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO webhook_endpoints (id, account_id, url, description, topics, metrics, filter, enabled, source, batch_size, rate_per_second, secret_enc, secret_hint,
            trusted_only, api_key_id)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15)",
    )
    .bind(id)
    .bind(account_id)
    .bind(&url)
    .bind(f.description.unwrap_or_default())
    .bind(&topics)
    .bind(f.metrics.unwrap_or_default())
    .bind(f.filter.flatten())
    .bind(f.enabled.unwrap_or(true))
    .bind(source)
    .bind(f.batch_size.unwrap_or(default_batch))
    .bind(f.rate_per_second.unwrap_or(10))
    .bind(crate::crypto::encrypt(&state.cfg.secret_key, &secret)?)
    .bind(super::secret_hint(&secret))
    .bind(f.trusted_only.unwrap_or(false))
    .bind(api_key_id)
    .execute(&state.db)
    .await?;
    super::forget(&state.db).await;
    audit(state, ctx, "webhook.create", Some(&id.to_string()), json!({"host": host(&url), "topics": topics, "source": source})).await;
    let who = match api_key_id {
        Some(k) => Who::key(account_id, k),
        None => Who::of(ctx),
    };
    let mut out = get_one(state, who, id).await?;
    out["secret"] = json!(secret);
    Ok(out)
}

/// Changing the URL starts a new revision: deliveries queued for the old URL are cancelled, and
/// changes captured before the edit never go to the new one. The edit applies only to the revision
/// the editor read (`revision` in the body, or the one read here), so a stale edit can't put an old
/// address back.
pub(super) async fn update(state: &AppState, ctx: &Ctx, id: Uuid, body: &Value) -> AppResult<Value> {
    let who = Who::of(ctx);
    let account_id = ctx.account_id;
    let ep = endpoint(state, who, id).await?;
    let f = parse_fields(body).await?;
    let read = match body.get("revision") {
        None | Some(Value::Null) => ep.revision,
        Some(v) => v.as_i64().ok_or_else(|| AppError::bad("revision must be a whole number"))? as i32,
    };
    let stale = || AppError::Conflict("this endpoint's address changed since you opened it; reload it and try again".into());
    if read != ep.revision {
        return Err(stale());
    }
    let turning_on = f.enabled == Some(true);
    let new_url = f.url.as_deref().is_some_and(|u| u != ep.url);
    let mut tx = state.db.begin().await?;
    let n = sqlx::query(
        "UPDATE webhook_endpoints SET url = COALESCE($3, url), description = COALESCE($4, description), topics = COALESCE($5, topics),
           metrics = COALESCE($6, metrics), filter = CASE WHEN $7 THEN $8 ELSE filter END, batch_size = COALESCE($9, batch_size),
           rate_per_second = COALESCE($10, rate_per_second), enabled = COALESCE($11, enabled), trusted_only = COALESCE($14, trusted_only),
           revision = revision + CASE WHEN $13 THEN 1 ELSE 0 END,
           revision_started_at = CASE WHEN $13 THEN clock_timestamp() ELSE revision_started_at END,
           disabled_reason = CASE WHEN $12 THEN NULL ELSE disabled_reason END,
           failing_since = CASE WHEN ($12 AND NOT enabled) OR $13 THEN NULL ELSE failing_since END,
           consecutive_failures = CASE WHEN ($12 AND NOT enabled) OR $13 THEN 0 ELSE consecutive_failures END,
           retry_at = CASE WHEN $12 OR $13 THEN NULL ELSE retry_at END,
           drop_alerted = CASE WHEN $13 THEN false ELSE drop_alerted END,
           updated_at = now()
         WHERE id = $1 AND account_id = $2 AND revision = $15",
    )
    .bind(id)
    .bind(account_id)
    .bind(&f.url)
    .bind(&f.description)
    .bind(&f.topics)
    .bind(&f.metrics)
    .bind(f.filter.is_some())
    .bind(f.filter.clone().flatten())
    .bind(f.batch_size)
    .bind(f.rate_per_second)
    .bind(f.enabled)
    .bind(turning_on)
    .bind(new_url)
    .bind(f.trusted_only)
    .bind(read)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if n == 0 {
        return Err(stale());
    }
    if new_url {
        sqlx::query(
            "WITH c AS (UPDATE webhook_deliveries SET status = 'cancelled', error = 'url_changed'
                        WHERE endpoint_id = $1 AND status = 'pending' AND revision < (SELECT revision FROM webhook_endpoints WHERE id = $1) RETURNING 1)
             UPDATE webhook_endpoints SET pending = GREATEST(0, pending - (SELECT count(*) FROM c)::int) WHERE id = $1",
        )
        .bind(id)
        .execute(&mut *tx)
        .await?;
    }
    if turning_on {
        // What was still waiting (inside its 24 hours) is tried again now.
        sqlx::query("UPDATE webhook_deliveries SET next_attempt_at = now() WHERE endpoint_id = $1 AND status = 'pending' AND next_attempt_at > now()")
            .bind(id)
            .execute(&mut *tx)
            .await?;
        deliver::wake(&mut tx, &[id]).await?;
    }
    tx.commit().await?;
    super::forget(&state.db).await;
    let fields: Vec<&str> = body.as_object().map(|o| o.keys().map(String::as_str).filter(|k| *k != "revision").collect()).unwrap_or_default();
    let mut details = json!({"fields": fields});
    if new_url {
        details["host"] = json!(host(f.url.as_deref().unwrap_or("")));
    }
    audit(state, ctx, "webhook.update", Some(&id.to_string()), details).await;
    get_one(state, who, id).await
}

pub(super) async fn delete(state: &AppState, ctx: &Ctx, id: Uuid) -> AppResult<()> {
    let ep = endpoint(state, Who::of(ctx), id).await?;
    let n = sqlx::query("DELETE FROM webhook_endpoints WHERE id = $1 AND account_id = $2").bind(id).bind(ctx.account_id).execute(&state.db).await?.rows_affected();
    if n == 0 {
        return Err(AppError::not_found("webhook endpoint"));
    }
    super::forget(&state.db).await;
    audit(state, ctx, "webhook.delete", Some(&id.to_string()), json!({"host": host(&ep.url)})).await;
    Ok(())
}

/// New signing secret; the old one keeps signing (as a second `v1=`) for 24 hours.
pub(super) async fn rotate(state: &AppState, ctx: &Ctx, id: Uuid) -> AppResult<Value> {
    let account_id = ctx.account_id;
    endpoint(state, Who::of(ctx), id).await?;
    let secret = super::new_secret();
    let until: DateTime<Utc> = sqlx::query_scalar(
        "UPDATE webhook_endpoints SET previous_secret_enc = secret_enc, previous_secret_until = now() + interval '24 hours', secret_enc = $3, secret_hint = $4, updated_at = now()
         WHERE id = $1 AND account_id = $2 RETURNING previous_secret_until",
    )
    .bind(id)
    .bind(account_id)
    .bind(crate::crypto::encrypt(&state.cfg.secret_key, &secret)?)
    .bind(super::secret_hint(&secret))
    .fetch_one(&state.db)
    .await?;
    audit(state, ctx, "webhook.rotate_secret", Some(&id.to_string()), json!({})).await;
    Ok(json!({"secret": secret, "secret_hint": super::secret_hint(&secret), "previous_secret_valid_until": until}))
}

#[derive(sqlx::FromRow)]
struct DeliveryRow {
    id: Uuid,
    event_id: Uuid,
    topic: String,
    status: String,
    is_test: bool,
    attempts: i32,
    response_code: Option<i32>,
    error: Option<String>,
    duration_ms: Option<i32>,
    next_attempt_at: DateTime<Utc>,
    last_attempt_at: Option<DateTime<Utc>>,
    delivered_at: Option<DateTime<Utc>>,
    created_at: DateTime<Utc>,
}

const DELIVERY_COLS: &str = "id, event_id, topic, status, is_test, attempts, response_code, error, duration_ms, next_attempt_at, last_attempt_at, delivered_at, created_at";

fn delivery_json(d: &DeliveryRow) -> Value {
    json!({"id": d.id, "event_id": d.event_id, "topic": d.topic, "status": d.status, "test": d.is_test, "attempts": d.attempts,
        "response_code": d.response_code, "error": d.error, "duration_ms": d.duration_ms,
        "next_attempt_at": (d.status == "pending").then_some(d.next_attempt_at), "last_attempt_at": d.last_attempt_at,
        "delivered_at": d.delivered_at, "created_at": d.created_at})
}

/// Send one sample event now and record it in the log. It goes out on the same one-at-a-time
/// lease as real deliveries, and its row is never one the delivery job would pick up.
pub(super) async fn send_test(state: &AppState, ctx: &Ctx, id: Uuid, topic: Option<&str>) -> AppResult<Value> {
    let account_id = ctx.account_id;
    endpoint(state, Who::of(ctx), id).await?;
    let ep: Option<Endpoint> = sqlx::query_as(&format!(
        "UPDATE webhook_endpoints SET lease_until = now() + interval '30 seconds'
         WHERE id = $1 AND account_id = $2 AND (lease_until IS NULL OR lease_until < now()) AND {} RETURNING {}",
        deliver::KEY_LIVE,
        deliver::ENDPOINT_COLS
    ))
    .bind(id)
    .bind(account_id)
    .fetch_optional(&state.db)
    .await?;
    let Some(ep) = ep else { return Err(AppError::Busy("DumbMail is sending to this endpoint right now; try the test again in a few seconds".into())) };
    let res = test_on_lease(state, account_id, &ep, topic).await;
    sqlx::query("UPDATE webhook_endpoints SET lease_until = NULL WHERE id = $1").bind(id).execute(&state.db).await?;
    res
}

async fn test_on_lease(state: &AppState, account_id: Uuid, ep: &Endpoint, topic: Option<&str>) -> AppResult<Value> {
    let topics: Vec<String> = sqlx::query_scalar("SELECT topics FROM webhook_endpoints WHERE id = $1").bind(ep.id).fetch_one(&state.db).await?;
    let topic = match topic {
        Some(t) if topics::is_topic(t) => t.to_string(),
        Some(t) => return Err(AppError::bad(format!("unknown topic {t:?}"))),
        None => topics.first().cloned().unwrap_or_else(|| "event.tracked".into()),
    };
    let event_id = Uuid::now_v7();
    let env = json!({"id": event_id, "topic": topic, "created_at": Utc::now(), "account_id": account_id, "api_version": super::API_VERSION,
        "source": "system", "test": true, "data": topics::sample_data(&topic, None)});
    let delivery_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO webhook_deliveries (id, account_id, endpoint_id, revision, event_id, topic, payload, is_test, status) VALUES ($1, $2, $3, $4, $5, $6, $7, true, 'sending')",
    )
    .bind(delivery_id)
    .bind(account_id)
    .bind(ep.id)
    .bind(ep.revision)
    .bind(event_id)
    .bind(&topic)
    .bind(&env)
    .execute(&state.db)
    .await?;
    let secrets = deliver::secrets(state, ep)?;
    let body = if ep.batch_size <= 1 { serde_json::to_vec(&env) } else { serde_json::to_vec(&json!([env])) }.map_err(|e| AppError::Internal(e.into()))?;
    let outcome = deliver::send(&ep.url, &secrets, (ep.batch_size <= 1).then_some(topic.as_str()), &body).await;
    // A test is tried once.
    sqlx::query(
        "UPDATE webhook_deliveries SET status = CASE WHEN $2 THEN 'delivered' ELSE 'failed' END, attempts = 1, last_attempt_at = now(),
           delivered_at = CASE WHEN $2 THEN now() END, response_code = $3, error = $4, duration_ms = $5 WHERE id = $1",
    )
    .bind(delivery_id)
    .bind(outcome.ok)
    .bind(outcome.code)
    .bind(outcome.error)
    .bind(outcome.ms)
    .execute(&state.db)
    .await?;
    let d: DeliveryRow = sqlx::query_as(&format!("SELECT {DELIVERY_COLS} FROM webhook_deliveries WHERE id = $1")).bind(delivery_id).fetch_one(&state.db).await?;
    Ok(json!({"ok": outcome.ok, "delivery": delivery_json(&d)}))
}

#[derive(Deserialize)]
pub(super) struct DeliveryQuery {
    pub status: Option<String>,
    pub topic: Option<String>,
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

pub(super) async fn deliveries(state: &AppState, who: Who, id: Uuid, q: &DeliveryQuery) -> AppResult<(Vec<Value>, i64)> {
    let account_id = who.account_id;
    endpoint(state, who, id).await?;
    let status = q.status.as_deref().filter(|s| !s.is_empty() && *s != "all");
    if status.is_some_and(|s| !matches!(s, "pending" | "sending" | "delivered" | "failed" | "dropped" | "cancelled")) {
        return Err(AppError::bad("status must be pending, sending, delivered, failed, dropped, cancelled or all"));
    }
    let topic = q.topic.as_deref().filter(|s| !s.is_empty());
    let limit = q.limit.unwrap_or(50).clamp(1, 200);
    let offset = q.offset.unwrap_or(0).clamp(0, 100_000);
    let rows: Vec<DeliveryRow> = sqlx::query_as(&format!(
        "SELECT {DELIVERY_COLS} FROM webhook_deliveries WHERE endpoint_id = $1 AND account_id = $2 AND ($3::text IS NULL OR status = $3) AND ($4::text IS NULL OR topic = $4)
         ORDER BY created_at DESC, id DESC LIMIT $5 OFFSET $6"
    ))
    .bind(id)
    .bind(account_id)
    .bind(status)
    .bind(topic)
    .bind(limit)
    .bind(offset)
    .fetch_all(&state.db)
    .await?;
    let total: i64 = sqlx::query_scalar("SELECT count(*) FROM webhook_deliveries WHERE endpoint_id = $1 AND account_id = $2 AND ($3::text IS NULL OR status = $3) AND ($4::text IS NULL OR topic = $4)")
        .bind(id)
        .bind(account_id)
        .bind(status)
        .bind(topic)
        .fetch_one(&state.db)
        .await?;
    Ok((rows.iter().map(delivery_json).collect(), total))
}

/// One delivery with its payload (personal data: for people allowed to read payloads).
pub(super) async fn delivery(state: &AppState, who: Who, id: Uuid, delivery_id: Uuid) -> AppResult<Value> {
    let account_id = who.account_id;
    endpoint(state, who, id).await?;
    let payload: Option<Value> = sqlx::query_scalar("SELECT payload FROM webhook_deliveries WHERE id = $1 AND endpoint_id = $2 AND account_id = $3")
        .bind(delivery_id)
        .bind(id)
        .bind(account_id)
        .fetch_optional(&state.db)
        .await?;
    let payload = payload.ok_or_else(|| AppError::not_found("delivery"))?;
    let d: DeliveryRow = sqlx::query_as(&format!("SELECT {DELIVERY_COLS} FROM webhook_deliveries WHERE id = $1")).bind(delivery_id).fetch_one(&state.db).await?;
    let mut out = delivery_json(&d);
    out["payload"] = payload;
    Ok(out)
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub(super) struct ResendBody {
    #[serde(default)]
    pub delivery_ids: Option<Vec<Uuid>>,
    #[serde(default)]
    pub since: Option<DateTime<Utc>>,
    #[serde(default)]
    pub until: Option<DateTime<Utc>>,
    /// failed (default), delivered or all
    #[serde(default)]
    pub status: Option<String>,
}

const MAX_RESEND: i64 = 10_000;

/// Queue earlier deliveries again (same event ids, so receivers can drop duplicates). Only
/// deliveries made for the endpoint's current URL; at most one resend a minute per endpoint.
pub(super) async fn resend(state: &AppState, ctx: &Ctx, id: Uuid, b: ResendBody) -> AppResult<Value> {
    let account_id = ctx.account_id;
    endpoint(state, Who::of(ctx), id).await?;
    if b.delivery_ids.as_ref().is_none_or(|v| v.is_empty()) && b.since.is_none() {
        return Err(AppError::bad("give delivery_ids, or a since time (and optionally until) to resend a range"));
    }
    if b.delivery_ids.as_ref().is_some_and(|v| v.len() > 1000) {
        return Err(AppError::bad("at most 1,000 delivery_ids at a time; use since/until for more"));
    }
    let status = b.status.as_deref().unwrap_or("failed");
    if !matches!(status, "failed" | "delivered" | "dropped" | "all") {
        return Err(AppError::bad("status must be failed, delivered, dropped or all"));
    }
    let enabled: bool = sqlx::query_scalar("SELECT enabled FROM webhook_endpoints WHERE id = $1").bind(id).fetch_one(&state.db).await?;
    if !enabled {
        return Err(AppError::bad("turn the endpoint on before resending"));
    }
    // The limit is the claim: two resends at once can't both get past it. A resend also lets an
    // endpoint that's backing off be tried right away, so this caps that at once a minute.
    let mut tx = state.db.begin().await?;
    let claimed: Option<i32> = sqlx::query_scalar(
        "UPDATE webhook_endpoints SET last_resend_at = now(), retry_at = NULL
         WHERE id = $1 AND account_id = $2 AND enabled AND (last_resend_at IS NULL OR last_resend_at < now() - interval '1 minute') RETURNING revision",
    )
    .bind(id)
    .bind(account_id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(revision) = claimed else { return Err(AppError::Busy("one resend a minute per endpoint; try again shortly".into())) };
    let n = sqlx::query(
        "UPDATE webhook_deliveries SET status = 'pending', next_attempt_at = now(), expires_at = now() + interval '24 hours', error = NULL
         WHERE id IN (SELECT id FROM webhook_deliveries WHERE endpoint_id = $1 AND account_id = $2 AND revision = $8
                        AND status IN ('failed', 'delivered', 'dropped') AND NOT is_test
                        AND ($3::uuid[] IS NULL OR id = ANY($3)) AND ($4::timestamptz IS NULL OR created_at >= $4) AND ($5::timestamptz IS NULL OR created_at <= $5)
                        AND ($6 = 'all' OR status = $6)
                      ORDER BY created_at LIMIT $7)",
    )
    .bind(id)
    .bind(account_id)
    .bind(b.delivery_ids.filter(|v| !v.is_empty()))
    .bind(b.since)
    .bind(b.until)
    .bind(status)
    .bind(MAX_RESEND)
    .bind(revision)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    sqlx::query("UPDATE webhook_endpoints SET pending = pending + $2 WHERE id = $1").bind(id).bind(n as i32).execute(&mut *tx).await?;
    deliver::wake(&mut tx, &[id]).await?;
    tx.commit().await?;
    audit(state, ctx, "webhook.resend", Some(&id.to_string()), json!({"resent": n, "status": status})).await;
    Ok(json!({"resent": n, "limit": MAX_RESEND}))
}

pub(super) async fn flow_secret_info(state: &AppState, account_id: Uuid) -> AppResult<Value> {
    let row: Option<(String, DateTime<Utc>)> = sqlx::query_as("SELECT secret_hint, created_at FROM webhook_signing_secrets WHERE account_id = $1")
        .bind(account_id)
        .fetch_optional(&state.db)
        .await?;
    Ok(match row {
        Some((hint, created_at)) => json!({"secret_hint": hint, "created_at": created_at}),
        None => json!({"secret_hint": null, "created_at": null}),
    })
}

pub(super) async fn rotate_flow_secret(state: &AppState, ctx: &Ctx) -> AppResult<Value> {
    let account_id = ctx.account_id;
    let secret = super::new_secret();
    sqlx::query(
        "INSERT INTO webhook_signing_secrets (account_id, secret_enc, secret_hint) VALUES ($1, $2, $3)
         ON CONFLICT (account_id) DO UPDATE SET secret_enc = EXCLUDED.secret_enc, secret_hint = EXCLUDED.secret_hint, created_at = now()",
    )
    .bind(account_id)
    .bind(crate::crypto::encrypt(&state.cfg.secret_key, &secret)?)
    .bind(super::secret_hint(&secret))
    .execute(&state.db)
    .await?;
    audit(state, ctx, "webhook.rotate_flow_secret", None, json!({})).await;
    Ok(json!({"secret": secret, "secret_hint": super::secret_hint(&secret)}))
}

/// The API key a request was made with (REST hooks belong to the key that made them). Dashboard
/// sessions can't use REST hooks: a 400 (the permission itself is checked by the route's rule,
/// which answers 403).
pub(super) fn key_of(ctx: &Ctx) -> AppResult<Uuid> {
    match (ctx.via_api_key, ctx.api_key_id) {
        (true, Some(k)) => Ok(k),
        _ => Err(AppError::bad("REST hooks are made with an API key (Zapier, Make or your own code); in the dashboard, use Settings → Webhooks")),
    }
}

/// REST hook subscribe: one topic, one URL, one event per request. The hook belongs to `key_id`.
pub(super) async fn subscribe(state: &AppState, ctx: &Ctx, key_id: Uuid, b: &Value) -> AppResult<Value> {
    let obj = b.as_object().ok_or_else(|| AppError::bad("send a JSON object"))?;
    let url = ["url", "target_url", "hookUrl"].iter().find_map(|k| obj.get(*k).and_then(|v| v.as_str())).ok_or_else(|| AppError::bad("url is required"))?;
    let topic = obj.get("topic").and_then(|v| v.as_str()).ok_or_else(|| AppError::bad("topic is required"))?;
    let source = match obj.get("source").and_then(|v| v.as_str()).unwrap_or("api") {
        s @ ("zapier" | "make" | "api") => s,
        _ => return Err(AppError::bad("source must be zapier, make or api")),
    };
    let mut body = Map::new();
    body.insert("url".into(), json!(url));
    body.insert("topics".into(), json!([topic]));
    body.insert("description".into(), json!(format!("{} hook", match source { "zapier" => "Zapier", "make" => "Make", _ => "API" })));
    if let Some(m) = obj.get("metric").and_then(|v| v.as_str()).filter(|m| !m.trim().is_empty()) {
        body.insert("metrics".into(), json!([m]));
    }
    for k in ["filter", "trusted_only"] {
        if let Some(v) = obj.get(k) {
            body.insert(k.into(), v.clone());
        }
    }
    let ep = create(state, ctx, &Value::Object(body), source, 1, Some(key_id)).await?;
    Ok(json!({"id": ep["id"], "url": ep["url"], "topic": topic, "metric": ep["metrics"].get(0), "secret": ep["secret"]}))
}

/// The key's own REST hooks.
pub(super) async fn hooks(state: &AppState, account_id: Uuid, key_id: Uuid) -> AppResult<Vec<Value>> {
    let rows: Vec<(Uuid, String, Vec<String>, Vec<String>, String, bool, DateTime<Utc>)> = sqlx::query_as(
        "SELECT id, url, topics, metrics, source, enabled, created_at FROM webhook_endpoints WHERE account_id = $1 AND api_key_id = $2 ORDER BY created_at",
    )
    .bind(account_id)
    .bind(key_id)
    .fetch_all(&state.db)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(id, url, topics, metrics, source, enabled, created_at)| {
            json!({"id": id, "url": url, "topic": topics.first(), "metric": metrics.first(), "source": source, "enabled": enabled, "created_at": created_at})
        })
        .collect())
}

/// Delete one of the key's own REST hooks. Other endpoints, including dashboard ones, aren't its to delete.
pub(super) async fn unsubscribe(state: &AppState, ctx: &Ctx, key_id: Uuid, id: Uuid) -> AppResult<()> {
    let n = sqlx::query("DELETE FROM webhook_endpoints WHERE id = $1 AND account_id = $2 AND api_key_id = $3").bind(id).bind(ctx.account_id).bind(key_id).execute(&state.db).await?.rows_affected();
    if n == 0 {
        return Err(AppError::not_found("hook"));
    }
    super::forget(&state.db).await;
    audit(state, ctx, "webhook.delete", Some(&id.to_string()), json!({"rest_hook": true})).await;
    Ok(())
}

fn envelope(id: Value, account_id: Uuid, topic: &str, source: &str, created_at: DateTime<Utc>, data: Value) -> Value {
    json!({"id": id, "topic": topic, "created_at": created_at, "account_id": account_id, "api_version": super::API_VERSION, "source": source, "data": data})
}

/// Sample events for a topic, newest first, shaped like real deliveries. For `event.tracked` with
/// a metric, the account's latest real events of that metric when it has any.
pub(super) async fn samples(state: &AppState, account_id: Uuid, topic: &str, metric: Option<&str>) -> AppResult<Value> {
    if !topics::is_topic(topic) {
        return Err(AppError::bad(format!("unknown topic {topic:?} (see GET /api/webhooks/topics)")));
    }
    if let (Some(metric), "event.tracked") = (metric, topic) {
        #[allow(clippy::type_complexity)]
        let rows: Vec<(Uuid, Value, Option<f64>, DateTime<Utc>, Option<String>, Option<Uuid>, bool, Uuid, Option<String>, Option<String>, Option<String>, Option<String>, Option<String>)> =
            sqlx::query_as(
                "SELECT e.id, e.properties, e.value, e.occurred_at, e.unique_id, e.message_id, e.from_browser, p.id, p.email, p.phone_number, p.external_id, p.first_name, p.last_name
                 FROM events e JOIN metrics m ON m.id = e.metric_id JOIN profiles p ON p.id = e.profile_id
                 WHERE e.account_id = $1 AND m.account_id = $1 AND m.name = $2 ORDER BY e.occurred_at DESC LIMIT 3",
            )
            .bind(account_id)
            .bind(metric.trim())
            .fetch_all(&state.db)
            .await?;
        if !rows.is_empty() {
            // Events don't record whether the API or DumbMail sent them; DumbMail's own metric
            // names are taken as its own.
            let not_browser = if crate::audience::is_reserved_metric(metric) { "system" } else { "api" };
            let out: Vec<Value> = rows
                .into_iter()
                .map(|r| {
                    envelope(json!(r.0), account_id, topic, if r.6 { "browser" } else { not_browser }, r.3, json!({
                        "event": {"id": r.0, "metric": metric.trim(), "properties": r.1, "value": r.2, "occurred_at": r.3, "unique_id": r.4, "message_id": r.5},
                        "profile": {"id": r.7, "email": r.8, "phone_number": r.9, "external_id": r.10, "first_name": r.11, "last_name": r.12},
                    }))
                })
                .collect();
            return Ok(Value::Array(out));
        }
    }
    let source = if topic == "form.submitted" { "browser" } else { "system" };
    Ok(json!([envelope(json!("00000000-0000-7000-8000-000000000000"), account_id, topic, source, Utc::now(), topics::sample_data(topic, metric))]))
}

// ---------- Handlers ----------

async fn topics_h(_ctx: Ctx) -> Json<Value> {
    Json(json!({"data": topics::list_json()}))
}

async fn list_h(State(state): State<AppState>, ctx: Ctx) -> AppResult<Json<Value>> {
    let rows = list(&state, Who::of(&ctx)).await?;
    let n = rows.len() as i64;
    Ok(Json(json!({"data": rows, "total": n})))
}

async fn create_h(State(state): State<AppState>, ctx: Ctx, ApiJson(b): ApiJson<Value>) -> AppResult<(StatusCode, Json<Value>)> {
    Ok((StatusCode::CREATED, Json(create(&state, &ctx, &b, if ctx.via_api_key { "api" } else { "dashboard" }, 50, None).await?)))
}

async fn get_h(State(state): State<AppState>, ctx: Ctx, Path(id): Path<Uuid>) -> AppResult<Json<Value>> {
    Ok(Json(get_one(&state, Who::of(&ctx), id).await?))
}

async fn patch_h(State(state): State<AppState>, ctx: Ctx, Path(id): Path<Uuid>, ApiJson(b): ApiJson<Value>) -> AppResult<Json<Value>> {
    Ok(Json(update(&state, &ctx, id, &b).await?))
}

async fn delete_h(State(state): State<AppState>, ctx: Ctx, Path(id): Path<Uuid>) -> AppResult<Json<Value>> {
    delete(&state, &ctx, id).await?;
    Ok(Json(json!({"ok": true})))
}

async fn rotate_h(State(state): State<AppState>, ctx: Ctx, Path(id): Path<Uuid>) -> AppResult<Json<Value>> {
    Ok(Json(rotate(&state, &ctx, id).await?))
}

#[derive(Deserialize, Default)]
struct TestBody {
    topic: Option<String>,
}

async fn test_h(State(state): State<AppState>, ctx: Ctx, Path(id): Path<Uuid>, body: axum::body::Bytes) -> AppResult<Json<Value>> {
    let b: TestBody = if body.iter().all(|c| c.is_ascii_whitespace()) { TestBody::default() } else { serde_json::from_slice(&body)? };
    Ok(Json(send_test(&state, &ctx, id, b.topic.as_deref()).await?))
}

async fn deliveries_h(State(state): State<AppState>, ctx: Ctx, Path(id): Path<Uuid>, Query(q): Query<DeliveryQuery>) -> AppResult<Json<Value>> {
    let (rows, total) = deliveries(&state, Who::of(&ctx), id, &q).await?;
    Ok(Json(json!({"data": rows, "total": total})))
}

async fn delivery_h(State(state): State<AppState>, ctx: Ctx, Path((id, delivery_id)): Path<(Uuid, Uuid)>) -> AppResult<Json<Value>> {
    Ok(Json(delivery(&state, Who::of(&ctx), id, delivery_id).await?))
}

async fn resend_h(State(state): State<AppState>, ctx: Ctx, Path(id): Path<Uuid>, ApiJson(b): ApiJson<ResendBody>) -> AppResult<Json<Value>> {
    Ok(Json(resend(&state, &ctx, id, b).await?))
}

async fn flow_secret_h(State(state): State<AppState>, ctx: Ctx) -> AppResult<Json<Value>> {
    Ok(Json(flow_secret_info(&state, ctx.account_id).await?))
}

async fn rotate_flow_secret_h(State(state): State<AppState>, ctx: Ctx) -> AppResult<Json<Value>> {
    Ok(Json(rotate_flow_secret(&state, &ctx).await?))
}

async fn subscribe_h(State(state): State<AppState>, ctx: Ctx, ApiJson(b): ApiJson<Value>) -> AppResult<(StatusCode, Json<Value>)> {
    let key = key_of(&ctx)?;
    Ok((StatusCode::CREATED, Json(subscribe(&state, &ctx, key, &b).await?)))
}

async fn hooks_h(State(state): State<AppState>, ctx: Ctx) -> AppResult<Json<Value>> {
    let key = key_of(&ctx)?;
    let rows = hooks(&state, ctx.account_id, key).await?;
    let n = rows.len();
    Ok(Json(json!({"data": rows, "total": n})))
}

async fn unsubscribe_h(State(state): State<AppState>, ctx: Ctx, Path(id): Path<Uuid>) -> AppResult<Json<Value>> {
    let key = key_of(&ctx)?;
    unsubscribe(&state, &ctx, key, id).await?;
    Ok(Json(json!({"ok": true})))
}

async fn me_h(State(state): State<AppState>, ctx: Ctx) -> AppResult<Json<Value>> {
    key_of(&ctx)?;
    let name: String = sqlx::query_scalar("SELECT name FROM accounts WHERE id = $1").bind(ctx.account_id).fetch_one(&state.db).await?;
    Ok(Json(json!({"account": {"id": ctx.account_id, "name": name}})))
}

#[derive(Deserialize)]
struct SampleQuery {
    topic: String,
    metric: Option<String>,
}

async fn samples_h(State(state): State<AppState>, ctx: Ctx, Query(q): Query<SampleQuery>) -> AppResult<Json<Value>> {
    key_of(&ctx)?;
    Ok(Json(samples(&state, ctx.account_id, &q.topic, q.metric.as_deref().filter(|m| !m.is_empty())).await?))
}
