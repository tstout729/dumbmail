//! REST API for integrations and the public inbound webhook URL.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Instant;

use axum::body::Body;
use axum::extract::{Path, Query, Request, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::Utc;
use once_cell::sync::Lazy;
use serde::Deserialize;
use serde_json::{json, Map, Value};
use uuid::Uuid;

use super::engine::{self, NewDelivery, Refused, TestSample, BODY_MAX_BYTES};
use super::model::{self, Changes};
use super::pull::Scrub;
use super::{recipes, signature};
use crate::api::ApiJson;
use crate::auth::Ctx;
use crate::error::{AppError, AppResult};
use crate::features::team::access::{self, public, rule, Perm, RouteRule};
use crate::features::team::audit;
use crate::state::AppState;

/// Who may do what (see `team::access`). Managers and up see integrations and their delivery log;
/// admins and the owner change them, run them and read stored payloads and script output. Only the
/// inbound webhook URL is public (it is checked by its token and the integration's signature).
pub static ROUTES: &[RouteRule] = &[
    public("POST", "/webhooks/in/{token}"),
    rule("GET", "/api/integrations", Perm::IntegrationsRead),
    rule("POST", "/api/integrations", Perm::IntegrationsManage),
    rule("POST", "/api/integrations/from-recipe", Perm::IntegrationsManage),
    rule("GET", "/api/integration-recipes", Perm::IntegrationsRead),
    rule("GET", "/api/integration-recipes/{key}", Perm::IntegrationsRead),
    rule("GET", "/api/integrations/{id}", Perm::IntegrationsRead),
    rule("PATCH", "/api/integrations/{id}", Perm::IntegrationsManage),
    rule("DELETE", "/api/integrations/{id}", Perm::IntegrationsManage),
    rule("POST", "/api/integrations/{id}/test", Perm::IntegrationsManage),
    rule("POST", "/api/integrations/{id}/status", Perm::IntegrationsManage),
    rule("POST", "/api/integrations/{id}/rotate-url", Perm::IntegrationsManage),
    rule("POST", "/api/integrations/{id}/pull", Perm::IntegrationsManage),
    rule("POST", "/api/integrations/{id}/backfill/cancel", Perm::IntegrationsManage),
    rule("GET", "/api/integrations/{id}/deliveries", Perm::IntegrationsRead),
    // Read: the log line. Payloads, headers and script output only with integrations.manage.
    rule("GET", "/api/integrations/{id}/deliveries/{delivery_id}", Perm::IntegrationsRead),
    rule("POST", "/api/integrations/{id}/replay", Perm::IntegrationsManage),
];

/// Whether this caller may see stored payloads, script output, saved state and the inbound URL.
pub fn can_manage(ctx: &Ctx) -> bool {
    access::allows(ctx, Perm::IntegrationsManage)
}

/// The integration as this caller may see it.
pub fn view_for(i: &model::Integration, state: &AppState, ctx: &Ctx) -> Value {
    if can_manage(ctx) { i.view(state) } else { i.view_limited(state) }
}

/// Audit-log a change to an integration (fixed fields only: never payloads or secret values).
pub async fn audit_change(state: &AppState, ctx: &Ctx, action: &str, id: Uuid, details: Value) {
    audit(state, ctx, action, Some(&id.to_string()), details).await;
}

/// What an update changed, for the audit log: field names, and which secrets were set or removed
/// (by name, never by value).
pub fn changed_fields(c: &Changes) -> Value {
    let mut fields = Vec::new();
    for (name, set) in [
        ("name", c.name.is_some()),
        ("script", c.script.is_some()),
        ("signature", c.signature.is_some()),
        ("settings", c.settings.is_some()),
        ("pull", c.pull.is_some()),
        ("state", c.state.is_some()),
        ("contacts_opted_in", c.contacts_opted_in.is_some()),
        ("message_id", c.message_id.is_some()),
        ("metrics", c.metrics.is_some()),
        ("redact", c.redact.is_some()),
    ] {
        if set {
            fields.push(json!(name));
        }
    }
    let mut set_secrets: Vec<String> = Vec::new();
    if c.signature_secret.is_some() {
        set_secrets.push("signing".into());
    }
    if let Some(cr) = &c.credentials {
        set_secrets.extend(cr.keys().map(|k| format!("credential {k}")));
    }
    json!({"fields": fields, "changed_values": set_secrets})
}

/// Webhooks one integration may receive per minute; past it the sender gets 429 and retries later.
pub const RATE_PER_MINUTE: u32 = 300;
/// Webhooks one account may receive per minute across its integrations.
pub const ACCOUNT_RATE_PER_MINUTE: u32 = 1200;
/// Rejected requests (bad signatures) logged per integration per minute; the rest are refused silently.
const REJECTED_LOGGED_PER_MINUTE: u32 = 30;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/webhooks/in/{token}", post(inbound))
        .route("/api/integrations", get(list_h).post(create_h))
        .route("/api/integrations/from-recipe", post(from_recipe_h))
        .route("/api/integration-recipes", get(recipes_h))
        .route("/api/integration-recipes/{key}", get(recipe_h))
        .route("/api/integrations/{id}", get(get_h).patch(update_h).delete(delete_h))
        .route("/api/integrations/{id}/test", post(test_h))
        .route("/api/integrations/{id}/status", post(status_h))
        .route("/api/integrations/{id}/rotate-url", post(rotate_h))
        .route("/api/integrations/{id}/pull", post(pull_h))
        .route("/api/integrations/{id}/backfill/cancel", post(cancel_backfill_h))
        .route("/api/integrations/{id}/deliveries", get(deliveries_h))
        .route("/api/integrations/{id}/deliveries/{delivery_id}", get(delivery_h))
        .route("/api/integrations/{id}/replay", post(replay_h))
}

// ---------- Inbound webhooks ----------

/// Fixed one-minute windows, in memory: accepted webhooks per integration and per account, and
/// rejected ones (bad signatures) per integration, counted separately so failures never use up
/// the allowance real deliveries need.
struct Windows {
    accepted: HashMap<Uuid, (Instant, u32)>,
    accounts: HashMap<Uuid, (Instant, u32)>,
    rejected: HashMap<Uuid, (Instant, u32)>,
}

static WINDOWS: Lazy<Mutex<Windows>> = Lazy::new(|| Mutex::new(Windows { accepted: HashMap::new(), accounts: HashMap::new(), rejected: HashMap::new() }));

fn bump(map: &mut HashMap<Uuid, (Instant, u32)>, id: Uuid, cap: u32) -> bool {
    if map.len() > 10_000 {
        map.retain(|_, (start, _)| start.elapsed().as_secs() < 60);
    }
    let e = map.entry(id).or_insert((Instant::now(), 0));
    if e.0.elapsed().as_secs() >= 60 {
        *e = (Instant::now(), 0);
    }
    if e.1 >= cap {
        return false;
    }
    e.1 += 1;
    true
}

/// Take an accepted-webhook slot for the integration and its account. False once either is full.
fn take_slot(id: Uuid, account: Uuid) -> bool {
    let mut w = WINDOWS.lock().unwrap();
    let integ_ok = w.accepted.get(&id).is_none_or(|(t, n)| t.elapsed().as_secs() >= 60 || *n < RATE_PER_MINUTE);
    let acct_ok = w.accounts.get(&account).is_none_or(|(t, n)| t.elapsed().as_secs() >= 60 || *n < ACCOUNT_RATE_PER_MINUTE);
    if !(integ_ok && acct_ok) {
        return false;
    }
    bump(&mut w.accepted, id, RATE_PER_MINUTE) && bump(&mut w.accounts, account, ACCOUNT_RATE_PER_MINUTE)
}

/// Count a rejected request; true while rejections are few enough to still be logged.
fn note_rejected(id: Uuid) -> bool {
    bump(&mut WINDOWS.lock().unwrap().rejected, id, REJECTED_LOGGED_PER_MINUTE)
}

fn reply(status: StatusCode, v: Value) -> Response {
    (status, Json(v)).into_response()
}

fn busy(msg: &str) -> Response {
    (StatusCode::TOO_MANY_REQUESTS, [("retry-after", "60")], Json(json!({"error": msg}))).into_response()
}

/// Headers a script may need. Credentials and cookies are never stored.
fn keep_headers(h: &HeaderMap, secret_header: Option<&str>) -> Value {
    let drop = ["authorization", "cookie", "proxy-authorization", "x-api-key"];
    let mut m = Map::new();
    for (k, v) in h.iter().take(60) {
        let name = k.as_str();
        if drop.contains(&name) || Some(name) == secret_header {
            continue;
        }
        let val: String = String::from_utf8_lossy(v.as_bytes()).chars().take(1000).collect();
        m.insert(name.to_string(), Value::String(val));
    }
    Value::Object(m)
}

async fn inbound(State(state): State<AppState>, Path(token): Path<String>, Query(mut query): Query<HashMap<String, String>>, req: Request) -> Response {
    if token.len() > 100 {
        return reply(StatusCode::NOT_FOUND, json!({"error": "unknown webhook URL"}));
    }
    let id: Option<Uuid> = match sqlx::query_scalar("SELECT id FROM integrations WHERE token_hash = $1").bind(crate::auth::sha256_hex(&token)).fetch_optional(&state.db).await {
        Ok(v) => v,
        Err(e) => return AppError::from(e).into_response(),
    };
    let Some(id) = id else { return reply(StatusCode::NOT_FOUND, json!({"error": "unknown webhook URL"})) };
    let integ = match model::load_any(&state, id).await {
        Ok(Some(i)) => i,
        Ok(None) => return reply(StatusCode::NOT_FOUND, json!({"error": "unknown webhook URL"})),
        Err(e) => return e.into_response(),
    };
    let (parts, body) = req.into_parts();
    let bytes = match axum::body::to_bytes(Body::new(body), BODY_MAX_BYTES).await {
        Ok(b) => b,
        Err(_) => return reply(StatusCode::PAYLOAD_TOO_LARGE, json!({"error": "the body is over 1 MB"})),
    };
    // The signature is checked before anything is spent: a caller without the secret never uses up
    // the integration's allowance, and its failures are only logged while they are few.
    let secret = integ.secret(&state).ok().flatten();
    // The URL as the app was given it, for apps that sign it (Square).
    let base_url = format!("{}/webhooks/in/{token}", state.cfg.public_url());
    let signed_url = match parts.uri.query() {
        Some(q) => format!("{base_url}?{q}"),
        None => base_url,
    };
    // Signature settings that can't be read refuse everything; they never mean "no check".
    let checked = integ
        .signature()
        .map_err(|e| format!("the integration's signature settings can't be read ({e}); fix them before webhooks are accepted"))
        .and_then(|sig| signature::verify(&sig, secret.as_deref(), &parts.headers, &query, &signed_url, &bytes, Utc::now().timestamp()).map(|_| sig));
    let sig = match checked {
        Ok(sig) => sig,
        Err(why) => {
            if note_rejected(id) {
                let msg = format!("signature check failed: {why}");
                let scrub = Scrub::new(integ.secret_values(&state).await);
                let _ = engine::store_delivery(&state, &integ, &scrub, NewDelivery::log("webhook", "error", Some(&msg))).await;
                let _ = sqlx::query("UPDATE integrations SET deliveries_failed = deliveries_failed + 1, last_delivery_at = now(), last_error = $2, last_error_at = now() WHERE id = $1").bind(id).bind(&msg).execute(&state.db).await;
            }
            return reply(StatusCode::UNAUTHORIZED, json!({"error": "signature check failed"}));
        }
    };
    if !take_slot(id, integ.account_id) {
        return busy(&format!("more than {RATE_PER_MINUTE} webhooks a minute; retry later"));
    }
    if let Some(q) = sig.secret_query() {
        query.remove(q);
    }
    let message_id = integ.message_id_of(&parts.headers, &bytes);
    let headers = keep_headers(&parts.headers, sig.secret_header().as_deref());
    let ct = parts.headers.get("content-type").and_then(|v| v.to_str().ok()).map(String::from);
    let body = String::from_utf8_lossy(&bytes).into_owned();
    let query = json!(query);
    let scrub = Scrub::new(integ.secret_values(&state).await);
    // A turned-off integration keeps only a log line: nothing of the payload is stored.
    let d = if integ.status == "enabled" {
        NewDelivery { source: "webhook", content_type: ct.as_deref(), headers: &headers, query: &query, body: Some(&body), request: None, status: "pending", error: None, replay_of: None, message_id, applied: None, dedupe_minutes: integ.dedupe_minutes() }
    } else {
        NewDelivery { message_id, ..NewDelivery::log("webhook", "ignored", Some("the integration is turned off; the payload wasn't kept")) }
    };
    match engine::store_delivery(&state, &integ, &scrub, d).await {
        Ok(d) => reply(StatusCode::OK, json!({"ok": true, "delivery_id": d})),
        // The app sent this message before (a retry): acknowledge it so it stops, and do nothing.
        Err(Refused::Duplicate(first)) => reply(StatusCode::OK, json!({"ok": true, "duplicate": true, "delivery_id": first})),
        Err(Refused::Busy(msg) | Refused::OverBudget(msg)) => busy(&msg),
        Err(Refused::Error(e)) => e.into_response(),
    }
}

// ---------- Shared by the REST API and MCP ----------

pub async fn list(state: &AppState, account_id: Uuid) -> AppResult<Value> {
    let rows = model::list(state, account_id).await?;
    Ok(json!({"data": rows.iter().map(|i| i.summary(state)).collect::<Vec<_>>()}))
}

pub async fn create(state: &AppState, ctx: &Ctx, c: Changes) -> AppResult<Value> {
    let details = changed_fields(&c);
    let i = model::create(state, ctx.account_id, None, c).await?;
    audit_change(state, ctx, "integration.create", i.id, json!({"name": i.name, "changed": details})).await;
    Ok(i.view(state))
}

pub async fn update(state: &AppState, ctx: &Ctx, id: Uuid, c: Changes) -> AppResult<Value> {
    let details = changed_fields(&c);
    let secrets = details["changed_values"].as_array().is_some_and(|a| !a.is_empty());
    let i = model::update(state, ctx.account_id, id, c).await?;
    audit_change(state, ctx, "integration.update", i.id, details.clone()).await;
    if secrets {
        audit_change(state, ctx, "integration.secrets_change", i.id, json!({"changed_values": details["changed_values"]})).await;
    }
    Ok(i.view(state))
}

pub async fn remove(state: &AppState, ctx: &Ctx, id: Uuid) -> AppResult<()> {
    let i = model::load(state, ctx.account_id, id).await?;
    model::delete(state, ctx.account_id, id).await?;
    audit_change(state, ctx, "integration.delete", id, json!({"name": i.name})).await;
    Ok(())
}

pub async fn set_status(state: &AppState, ctx: &Ctx, id: Uuid, status: &str) -> AppResult<Value> {
    let i = model::set_status(state, ctx.account_id, id, status).await?;
    audit_change(state, ctx, "integration.update", id, json!({"fields": ["status"], "status": i.status})).await;
    Ok(i.view(state))
}

pub async fn rotate(state: &AppState, ctx: &Ctx, id: Uuid) -> AppResult<Value> {
    let i = model::rotate_url(state, ctx.account_id, id).await?;
    audit_change(state, ctx, "integration.secrets_change", id, json!({"changed_values": ["inbound URL"]})).await;
    Ok(i.view(state))
}

pub async fn pull(state: &AppState, ctx: &Ctx, id: Uuid, since: Option<chrono::DateTime<Utc>>, resume: bool) -> AppResult<Value> {
    let i = model::load(state, ctx.account_id, id).await?;
    let out = engine::start_pull(state, &i, since, resume).await?;
    if since.is_some() || resume {
        audit_change(state, ctx, "integration.backfill", id, json!({"since": since, "resumed": resume})).await;
    }
    Ok(out)
}

pub async fn cancel_backfill(state: &AppState, ctx: &Ctx, id: Uuid) -> AppResult<Value> {
    let i = model::load(state, ctx.account_id, id).await?;
    let out = engine::cancel_backfill(state, &i).await?;
    audit_change(state, ctx, "integration.backfill_cancel", id, json!({})).await;
    Ok(out)
}

pub async fn from_recipe(state: &AppState, ctx: &Ctx, key: &str, name: Option<String>, create_flows: bool) -> AppResult<Value> {
    let (i, flows) = recipes::create(state, ctx.account_id, key, name, create_flows).await?;
    audit_change(state, ctx, "integration.create", i.id, json!({"name": i.name, "recipe": key, "flows": flows.len()})).await;
    let r = recipes::get(key).map(|r| r.view(false)).unwrap_or(Value::Null);
    Ok(json!({"integration": i.view(state), "flows": flows, "setup": r["setup"], "credentials_needed": r["credentials"], "signature_secret": r["signature_secret"]}))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TestBody {
    /// Try a script without saving it.
    #[serde(default)]
    pub script: Option<String>,
    #[serde(default)]
    pub payload: Option<Value>,
    #[serde(default)]
    pub headers: Option<Value>,
    #[serde(default)]
    pub query: Option<Value>,
    #[serde(default)]
    pub source: Option<String>,
    /// Fetch the first page of the pull now and run the script on it.
    #[serde(default)]
    pub pull: bool,
}

pub async fn test(state: &AppState, account_id: Uuid, id: Uuid, b: TestBody) -> AppResult<Value> {
    let i = model::load(state, account_id, id).await?;
    if let Some(s) = &b.script {
        if s.len() > super::sandbox::SCRIPT_MAX_BYTES {
            return Err(AppError::bad("the script is too long"));
        }
    }
    if b.source.as_deref().is_some_and(|s| !matches!(s, "webhook" | "pull" | "fetch" | "test")) {
        return Err(AppError::bad("source must be webhook, pull, fetch or test"));
    }
    let sample = if b.pull {
        TestSample::Pull
    } else {
        let payload = b.payload.ok_or_else(|| AppError::bad("give a sample `payload` (or pull: true to fetch a page)"))?;
        if serde_json::to_string(&payload).map(|s| s.len()).unwrap_or(0) > BODY_MAX_BYTES {
            return Err(AppError::bad("the sample payload is over 1 MB"));
        }
        TestSample::Payload { payload, headers: b.headers, query: b.query, source: b.source }
    };
    engine::test(state, &i, b.script.as_deref(), sample).await
}

#[derive(Debug, Deserialize)]
pub struct DeliveryQuery {
    pub status: Option<String>,
    pub source: Option<String>,
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

pub async fn deliveries(state: &AppState, account_id: Uuid, id: Uuid, q: &DeliveryQuery) -> AppResult<Value> {
    model::load(state, account_id, id).await?;
    let limit = q.limit.unwrap_or(50).clamp(1, 200);
    let offset = q.offset.unwrap_or(0).clamp(0, 100_000);
    let rows: Vec<(Uuid, String, String, Option<String>, i32, Option<chrono::DateTime<Utc>>, Option<i32>, Option<Value>, Option<Uuid>, chrono::DateTime<Utc>, Option<chrono::DateTime<Utc>>, Option<i32>)> = sqlx::query_as(
        "SELECT id, source, status, error, attempts, next_attempt_at, duration_ms, result->'counts', replay_of, created_at, processed_at, length(body)
         FROM integration_deliveries WHERE integration_id = $1 AND account_id = $2
           AND ($3::text IS NULL OR status = $3) AND ($4::text IS NULL OR source = $4)
         ORDER BY created_at DESC LIMIT $5 OFFSET $6",
    )
    .bind(id)
    .bind(account_id)
    .bind(&q.status)
    .bind(&q.source)
    .bind(limit)
    .bind(offset)
    .fetch_all(&state.db)
    .await?;
    let total: i64 = sqlx::query_scalar("SELECT count(*) FROM integration_deliveries WHERE integration_id = $1 AND ($2::text IS NULL OR status = $2) AND ($3::text IS NULL OR source = $3)")
        .bind(id)
        .bind(&q.status)
        .bind(&q.source)
        .fetch_one(&state.db)
        .await?;
    let data: Vec<Value> = rows
        .into_iter()
        .map(|r| json!({"id": r.0, "source": r.1, "status": r.2, "error": r.3, "attempts": r.4, "next_attempt_at": r.5, "duration_ms": r.6, "counts": r.7, "replay_of": r.8, "created_at": r.9, "processed_at": r.10, "has_payload": r.11.is_some(), "replayable": r.11.is_some()}))
        .collect();
    Ok(json!({"data": data, "total": total}))
}

pub async fn delivery(state: &AppState, account_id: Uuid, id: Uuid, delivery_id: Uuid, full: bool) -> AppResult<Value> {
    model::load(state, account_id, id).await?;
    let r: Option<(Uuid, String, String, Option<String>, Value, Value, Option<String>, Option<Value>, i32, Option<chrono::DateTime<Utc>>, Option<String>, Option<Value>, Option<i32>, Option<Uuid>, chrono::DateTime<Utc>, Option<chrono::DateTime<Utc>>)> = sqlx::query_as(
        "SELECT id, source, status, content_type, headers, query, body, request, attempts, next_attempt_at, error, result, duration_ms, replay_of, created_at, processed_at
         FROM integration_deliveries WHERE id = $1 AND integration_id = $2 AND account_id = $3",
    )
    .bind(delivery_id)
    .bind(id)
    .bind(account_id)
    .fetch_optional(&state.db)
    .await?;
    let r = r.ok_or_else(|| AppError::not_found("delivery"))?;
    if !full {
        // The log line only: payloads, headers, the request and script output can hold personal
        // data and need integrations.manage.
        return Ok(json!({
            "id": r.0, "source": r.1, "status": r.2, "attempts": r.8, "next_attempt_at": r.9, "error": r.10,
            "counts": r.11.as_ref().map(|x| x["counts"].clone()), "duration_ms": r.12, "replay_of": r.13,
            "created_at": r.14, "processed_at": r.15, "has_payload": r.6.is_some(), "payload_hidden": true,
        }));
    }
    let payload = r.6.as_deref().map(|b| engine::parse_body(r.3.as_deref(), b));
    Ok(json!({
        "id": r.0, "source": r.1, "status": r.2, "content_type": r.3, "headers": r.4, "query": r.5, "payload": payload, "body": r.6,
        "request": r.7, "attempts": r.8, "next_attempt_at": r.9, "error": r.10, "result": r.11, "duration_ms": r.12, "replay_of": r.13,
        "created_at": r.14, "processed_at": r.15,
    }))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReplayBody {
    #[serde(default)]
    pub delivery_ids: Vec<Uuid>,
    /// Every delivery that failed for good (up to 500).
    #[serde(default)]
    pub all_failed: bool,
}

pub async fn replay(state: &AppState, ctx: &Ctx, id: Uuid, b: &ReplayBody, dry: bool) -> AppResult<Value> {
    let i = model::load(state, ctx.account_id, id).await?;
    if b.delivery_ids.is_empty() && !b.all_failed {
        return Err(AppError::bad("give delivery_ids, or all_failed: true"));
    }
    if i.status != "enabled" && !dry {
        return Err(AppError::Conflict("turn the integration on before replaying".into()));
    }
    let out = engine::replay(state, &i, &b.delivery_ids, b.all_failed, dry).await?;
    if !dry {
        audit_change(state, ctx, "integration.replay", id, json!({"replayed": out["replayed"], "all_failed": b.all_failed})).await;
    }
    Ok(out)
}

pub fn parse_since(s: &str) -> AppResult<chrono::DateTime<Utc>> {
    super::actions::parse_time(s).ok_or_else(|| AppError::bad(format!("{s:?} isn't a date (use YYYY-MM-DD)")))
}

// ---------- Handlers ----------

async fn list_h(State(state): State<AppState>, ctx: Ctx) -> AppResult<Json<Value>> {
    Ok(Json(list(&state, ctx.account_id).await?))
}

async fn create_h(State(state): State<AppState>, ctx: Ctx, ApiJson(c): ApiJson<Changes>) -> AppResult<Json<Value>> {
    Ok(Json(create(&state, &ctx, c).await?))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FromRecipe {
    recipe: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    create_flows: Option<bool>,
}

async fn from_recipe_h(State(state): State<AppState>, ctx: Ctx, ApiJson(b): ApiJson<FromRecipe>) -> AppResult<Json<Value>> {
    Ok(Json(from_recipe(&state, &ctx, &b.recipe, b.name, b.create_flows.unwrap_or(true)).await?))
}

async fn recipes_h(_ctx: Ctx) -> Json<Value> {
    Json(json!({"data": recipes::all().iter().map(|r| r.view(false)).collect::<Vec<_>>()}))
}

async fn recipe_h(_ctx: Ctx, Path(key): Path<String>) -> AppResult<Json<Value>> {
    Ok(Json(recipes::get(&key).ok_or_else(|| AppError::not_found("recipe"))?.view(true)))
}

async fn get_h(State(state): State<AppState>, ctx: Ctx, Path(id): Path<Uuid>) -> AppResult<Json<Value>> {
    Ok(Json(view_for(&model::load(&state, ctx.account_id, id).await?, &state, &ctx)))
}

async fn update_h(State(state): State<AppState>, ctx: Ctx, Path(id): Path<Uuid>, ApiJson(c): ApiJson<Changes>) -> AppResult<Json<Value>> {
    Ok(Json(update(&state, &ctx, id, c).await?))
}

async fn delete_h(State(state): State<AppState>, ctx: Ctx, Path(id): Path<Uuid>) -> AppResult<Json<Value>> {
    remove(&state, &ctx, id).await?;
    Ok(Json(json!({"ok": true})))
}

async fn test_h(State(state): State<AppState>, ctx: Ctx, Path(id): Path<Uuid>, ApiJson(b): ApiJson<TestBody>) -> AppResult<Json<Value>> {
    Ok(Json(test(&state, ctx.account_id, id, b).await?))
}

#[derive(Deserialize)]
struct StatusBody {
    status: String,
}

async fn status_h(State(state): State<AppState>, ctx: Ctx, Path(id): Path<Uuid>, ApiJson(b): ApiJson<StatusBody>) -> AppResult<Json<Value>> {
    Ok(Json(set_status(&state, &ctx, id, &b.status).await?))
}

async fn rotate_h(State(state): State<AppState>, ctx: Ctx, Path(id): Path<Uuid>) -> AppResult<Json<Value>> {
    Ok(Json(rotate(&state, &ctx, id).await?))
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct PullBody {
    #[serde(default)]
    backfill_since: Option<String>,
    /// Continue a backfill that stopped at its page limit or failed.
    #[serde(default)]
    resume_backfill: bool,
}

async fn pull_h(State(state): State<AppState>, ctx: Ctx, Path(id): Path<Uuid>, body: axum::body::Bytes) -> AppResult<Json<Value>> {
    let b: PullBody = if body.iter().all(u8::is_ascii_whitespace) { PullBody::default() } else { serde_json::from_slice(&body)? };
    let since = b.backfill_since.as_deref().map(parse_since).transpose()?;
    Ok(Json(pull(&state, &ctx, id, since, b.resume_backfill).await?))
}

async fn cancel_backfill_h(State(state): State<AppState>, ctx: Ctx, Path(id): Path<Uuid>) -> AppResult<Json<Value>> {
    Ok(Json(cancel_backfill(&state, &ctx, id).await?))
}

async fn deliveries_h(State(state): State<AppState>, ctx: Ctx, Path(id): Path<Uuid>, Query(q): Query<DeliveryQuery>) -> AppResult<Json<Value>> {
    Ok(Json(deliveries(&state, ctx.account_id, id, &q).await?))
}

async fn delivery_h(State(state): State<AppState>, ctx: Ctx, Path((id, delivery_id)): Path<(Uuid, Uuid)>) -> AppResult<Json<Value>> {
    Ok(Json(delivery(&state, ctx.account_id, id, delivery_id, can_manage(&ctx)).await?))
}

async fn replay_h(State(state): State<AppState>, ctx: Ctx, Path(id): Path<Uuid>, ApiJson(b): ApiJson<ReplayBody>) -> AppResult<Json<Value>> {
    Ok(Json(replay(&state, &ctx, id, &b, false).await?))
}
