//! Move from Klaviyo in one step: connect a read-only Klaviyo key, see what's there, import profiles
//! with their exact consent, lists, segments, events, templates, campaigns, flows and forms, then prove
//! nothing was lost with a side-by-side report. An optional hourly sync keeps pulling changes (above all,
//! unsubscribes) until the customer switches over. Guide: docs/MOVING_FROM_KLAVIYO.md.

pub mod client;
pub mod consent;
pub mod flows;
pub mod report;
pub mod runner;
pub mod segments;
pub mod store;
pub mod survey;
pub mod templates;

use async_trait::async_trait;
use axum::extract::{Query, State};
use axum::http::header;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::Utc;
use serde::Deserialize;
use serde_json::{json, Value};
use uuid::Uuid;

use super::{Feature, McpDoc};
use crate::api::ApiJson;
use crate::auth::Ctx;
use crate::error::{AppError, AppResult};
use crate::mcp::{obj, tool};
use crate::state::AppState;
use client::{Client, KlaviyoError, SCOPES};
use store::{Import, CHOOSABLE, JOB_RUN, JOB_SURVEY, STEPS};

pub use client::set_api_base;

const DOC: &str = include_str!("../../../docs/MOVING_FROM_KLAVIYO.md");

pub struct Module;

// ---------- Actions shared by the REST API and the MCP tools ----------

fn hint(key: &str) -> String {
    let tail: String = key.chars().rev().take(4).collect::<Vec<_>>().into_iter().rev().collect();
    format!("pk_…{tail}")
}

pub async fn status_json(state: &AppState, account_id: Uuid) -> AppResult<Value> {
    let imp = store::latest(&state.db, account_id).await?;
    let issues: Vec<(String, i64)> = match &imp {
        Some(i) => sqlx::query_as("SELECT kind, count(*) FROM klaviyo_import_issues WHERE import_id = $1 GROUP BY 1").bind(i.id).fetch_all(&state.db).await?,
        None => Vec::new(),
    };
    let checklist = report::checklist(state, imp.as_ref(), account_id).await?;
    let worst_case = "Up to an hour, plus the time a sync takes. Before a scheduled campaign, a sync also runs within 30 minutes of its send time. Use Sync now right before sending anything by hand.";
    Ok(json!({
        "import": imp.as_ref().map(|i| {
            let mut v = serde_json::to_value(i).unwrap_or(json!({}));
            v["key_stored"] = json!(i.api_key_enc.is_some());
            v
        }),
        "issues": issues.into_iter().map(|(k, n)| (k, json!(n))).collect::<serde_json::Map<_, _>>(),
        "checklist": checklist,
        "steps": STEPS,
        "choosable": CHOOSABLE,
        "scopes": SCOPES.iter().map(|(label, scope)| json!({"label": label, "scope": scope})).collect::<Vec<_>>(),
        "sync_worst_case_delay": worst_case,
    }))
}

pub async fn connect(state: &AppState, account_id: Uuid, api_key: &str) -> AppResult<Value> {
    let key = api_key.trim();
    if key.is_empty() || key.len() > 200 || key.chars().any(char::is_whitespace) {
        return Err(AppError::bad("Paste your Klaviyo private API key (it starts with pk_)."));
    }
    if !key.starts_with("pk_") {
        return Err(AppError::bad("That isn't a Klaviyo private API key. Private keys start with pk_; the 6-character public key won't work."));
    }
    if let Some(cur) = store::latest(&state.db, account_id).await? {
        if cur.is_active() {
            return Err(AppError::Conflict("A Klaviyo import is already set up. Cancel it before connecting another key.".into()));
        }
    }
    let mut k = Client::new(state.http.clone(), key);
    let acct = match k.get("/api/accounts").await {
        Ok(v) => v,
        Err(KlaviyoError::MissingScope(_)) => {
            return Err(AppError::bad("The key works but can't read your Klaviyo account. Give it read access to Accounts (and the other read scopes listed) and try again."))
        }
        Err(e @ KlaviyoError::BadKey) => return Err(AppError::bad(e.to_string())),
        Err(e) => return Err(AppError::BadRequest(format!("Couldn't check the key with Klaviyo: {e}"))),
    };
    let a = &acct["data"][0];
    let info = json!({
        "id": a["id"], "name": a["attributes"]["contact_information"]["organization_name"],
        "timezone": a["attributes"]["timezone"], "currency": a["attributes"]["preferred_currency"],
        "website": a["attributes"]["contact_information"]["website_url"], "test_account": a["attributes"]["test_account"],
    });
    let enc = crate::crypto::encrypt(&state.cfg.secret_key, key)?;
    let id = Uuid::now_v7();
    sqlx::query("INSERT INTO klaviyo_imports (id, account_id, status, api_key_enc, key_hint, klaviyo_account) VALUES ($1, $2, 'surveying', $3, $4, $5)")
        .bind(id)
        .bind(account_id)
        .bind(enc)
        .bind(hint(key))
        .bind(&info)
        .execute(&state.db)
        .await?;
    store::enqueue(&state.db, JOB_SURVEY, id, None).await?;
    status_json(state, account_id).await
}

#[derive(Debug, Default, Deserialize)]
pub struct StartBody {
    #[serde(default)]
    pub confirm: bool,
    /// Parts to bring across (profiles, consent and suppressions always come): lists, segments, templates,
    /// campaigns, flows, forms, events. Default all.
    #[serde(default)]
    pub what: Option<Vec<String>>,
    /// First day of event history to import (YYYY-MM-DD), or "all". Default two years ago.
    #[serde(default)]
    pub events_from: Option<String>,
    /// Klaviyo metric names or ids whose events to import. Default: orders, checkouts, product views,
    /// carts and email/SMS engagement.
    #[serde(default)]
    pub metrics: Option<Vec<String>>,
    /// Keep pulling changes from Klaviyo every hour after the import.
    #[serde(default)]
    pub sync: Option<bool>,
}

fn options(imp: &Import, b: &StartBody) -> AppResult<Value> {
    let what = match &b.what {
        Some(w) => {
            if w.len() > 20 {
                return Err(AppError::bad("too many parts in `what`"));
            }
            for x in w {
                if !CHOOSABLE.contains(&x.as_str()) {
                    return Err(AppError::bad(format!("`what` can include {}; profiles and consent always come across", CHOOSABLE.join(", "))));
                }
            }
            json!(w)
        }
        None => json!(CHOOSABLE),
    };
    let events_from = match b.events_from.as_deref().map(str::trim) {
        None | Some("") => (imp.created_at - chrono::Duration::days(730)).format("%Y-%m-%d").to_string(),
        Some("all") => "all".into(),
        Some(d) => {
            let day = chrono::NaiveDate::parse_from_str(d, "%Y-%m-%d").map_err(|_| AppError::bad("events_from must be YYYY-MM-DD or \"all\""))?;
            if day > Utc::now().date_naive() {
                return Err(AppError::bad("events_from can't be in the future"));
            }
            d.to_string()
        }
    };
    let metrics = match &b.metrics {
        Some(m) if m.len() > 200 => return Err(AppError::bad("choose at most 200 metrics")),
        Some(m) if m.iter().any(|x| x.trim().is_empty() || x.len() > 128) => return Err(AppError::bad("metric names must be 1-128 characters")),
        Some(m) => json!(m),
        None => Value::Null,
    };
    Ok(json!({"what": what, "events_from": events_from, "metrics": metrics}))
}

pub async fn start(state: &AppState, account_id: Uuid, b: StartBody) -> AppResult<Value> {
    let imp = store::current(&state.db, account_id).await?;
    match imp.status.as_str() {
        "ready" => {}
        "surveying" => return Err(AppError::Conflict("Still looking at your Klaviyo account. Try again in a minute.".into())),
        s => return Err(AppError::Conflict(format!("This import is {s}. Connect your Klaviyo key again to run another."))),
    }
    let missing: Vec<String> = imp.survey.as_ref().map(|s| s["missing_scopes"].as_array().into_iter().flatten().filter_map(|x| x.as_str().map(String::from)).collect()).unwrap_or_default();
    if missing.iter().any(|m| m == "profiles:read") {
        return Err(AppError::bad("The key can't read profiles. Give it read access to Profiles and connect again."));
    }
    let opts = options(&imp, &b)?;
    if !b.confirm {
        return Ok(json!({
            "dry_run": true,
            "options": opts,
            "survey": imp.survey,
            "missing_scopes": missing,
            "message": "Nothing has been imported yet. Profiles, consent and suppressions always come across; nobody is emailed or texted, no flow starts and imported flows stay drafts. Call again with confirm=true to start.",
        }));
    }
    sqlx::query(
        "UPDATE klaviyo_imports SET status = 'importing', options = $2, started_at = now(), phase = NULL, cursor = NULL, step_state = '{}', progress = '{}',
            sync_enabled = COALESCE($3, sync_enabled), updated_at = now() WHERE id = $1 AND status = 'ready'",
    )
    .bind(imp.id)
    .bind(&opts)
    .bind(b.sync)
    .execute(&state.db)
    .await?;
    store::enqueue(&state.db, JOB_RUN, imp.id, None).await?;
    status_json(state, account_id).await
}

pub async fn pause(state: &AppState, account_id: Uuid, resume: bool) -> AppResult<Value> {
    let imp = store::current(&state.db, account_id).await?;
    let (from, to) = if resume { ("paused", "importing") } else { ("importing", "paused") };
    let n = sqlx::query("UPDATE klaviyo_imports SET status = $3, updated_at = now() WHERE id = $1 AND status = $2").bind(imp.id).bind(from).bind(to).execute(&state.db).await?.rows_affected();
    if n == 0 {
        return Err(AppError::Conflict(format!("This import is {}, so it can't be {}.", imp.status, if resume { "resumed" } else { "paused" })));
    }
    if resume {
        store::enqueue(&state.db, JOB_RUN, imp.id, None).await?;
    }
    status_json(state, account_id).await
}

pub async fn cancel(state: &AppState, account_id: Uuid) -> AppResult<Value> {
    let imp = store::current(&state.db, account_id).await?;
    if imp.is_active() {
        store::close(&state.db, imp.id, "cancelled", None).await?;
    } else if imp.api_key_enc.is_some() || imp.sync_enabled {
        sqlx::query("UPDATE klaviyo_imports SET sync_enabled = false, api_key_enc = NULL, updated_at = now() WHERE id = $1").bind(imp.id).execute(&state.db).await?;
    } else {
        return Err(AppError::Conflict(format!("This import is already {}.", imp.status)));
    }
    status_json(state, account_id).await
}

pub async fn set_sync(state: &AppState, account_id: Uuid, enabled: bool) -> AppResult<Value> {
    let imp = store::current(&state.db, account_id).await?;
    if enabled {
        if imp.api_key_enc.is_none() {
            return Err(AppError::Conflict("The Klaviyo key was deleted when the import finished. Connect it again to turn on the sync.".into()));
        }
        if !matches!(imp.status.as_str(), "ready" | "importing" | "paused" | "done" | "syncing") {
            return Err(AppError::Conflict(format!("This import is {}.", imp.status)));
        }
        sqlx::query("UPDATE klaviyo_imports SET sync_enabled = true, updated_at = now() WHERE id = $1").bind(imp.id).execute(&state.db).await?;
    } else {
        // Turning it off at the end deletes the key (a running sync pass deletes it when it ends).
        sqlx::query("UPDATE klaviyo_imports SET sync_enabled = false, api_key_enc = CASE WHEN status = 'done' THEN NULL ELSE api_key_enc END, updated_at = now() WHERE id = $1")
            .bind(imp.id)
            .execute(&state.db)
            .await?;
    }
    status_json(state, account_id).await
}

pub async fn sync_now(state: &AppState, account_id: Uuid) -> AppResult<Value> {
    let imp = store::current(&state.db, account_id).await?;
    match imp.status.as_str() {
        "syncing" => return Err(AppError::Conflict("A sync is already running.".into())),
        "done" if imp.sync_enabled && imp.api_key_enc.is_some() => {}
        "done" => return Err(AppError::Conflict("The ongoing sync is off. Turn it on (while the Klaviyo key is still stored) or connect the key again.".into())),
        s => return Err(AppError::Conflict(format!("The import is {s}; sync runs after it finishes."))),
    }
    runner::start_sync(&state.db, imp.id).await?;
    status_json(state, account_id).await
}

pub async fn comparison(state: &AppState, account_id: Uuid) -> AppResult<Value> {
    let imp = store::current(&state.db, account_id).await?;
    report::build(state, &imp).await
}

#[derive(Debug, Default, Deserialize)]
pub struct IssueQuery {
    pub step: Option<String>,
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

pub async fn issues(state: &AppState, account_id: Uuid, q: &IssueQuery) -> AppResult<Value> {
    let imp = store::current(&state.db, account_id).await?;
    let limit = q.limit.unwrap_or(100).clamp(1, 500);
    let offset = q.offset.unwrap_or(0).max(0);
    let rows: Vec<(i64, String, String, String, Option<String>, Option<Uuid>, String, chrono::DateTime<Utc>)> = sqlx::query_as(
        "SELECT id, step, kind, item, klaviyo_id, local_id, message, created_at FROM klaviyo_import_issues
         WHERE import_id = $1 AND ($2::text IS NULL OR step = $2) ORDER BY id LIMIT $3 OFFSET $4",
    )
    .bind(imp.id)
    .bind(&q.step)
    .bind(limit)
    .bind(offset)
    .fetch_all(&state.db)
    .await?;
    let total: i64 = sqlx::query_scalar("SELECT count(*) FROM klaviyo_import_issues WHERE import_id = $1 AND ($2::text IS NULL OR step = $2)").bind(imp.id).bind(&q.step).fetch_one(&state.db).await?;
    let data: Vec<Value> = rows
        .into_iter()
        .map(|(id, step, kind, item, kid, local, message, at)| json!({"id": id, "step": step, "kind": kind, "item": item, "klaviyo_id": kid, "local_id": local, "message": message, "created_at": at}))
        .collect();
    Ok(json!({"data": data, "total": total, "over_limit": imp.step_state["issues_over_limit"]}))
}

pub async fn campaign_history(state: &AppState, account_id: Uuid, q: &IssueQuery) -> AppResult<Value> {
    let limit = q.limit.unwrap_or(50).clamp(1, 200);
    let offset = q.offset.unwrap_or(0).max(0);
    let rows: Vec<(Uuid, String, String, String, Option<chrono::DateTime<Utc>>, Value)> = sqlx::query_as(
        "SELECT c.id, c.name, c.channel, c.status, c.sent_at, r.detail FROM klaviyo_refs r JOIN campaigns c ON c.id = r.local_id
         WHERE r.account_id = $1 AND r.kind = 'campaign_message' ORDER BY c.sent_at DESC NULLS LAST, c.created_at DESC LIMIT $2 OFFSET $3",
    )
    .bind(account_id)
    .bind(limit)
    .bind(offset)
    .fetch_all(&state.db)
    .await?;
    let total: i64 = sqlx::query_scalar("SELECT count(*) FROM klaviyo_refs r JOIN campaigns c ON c.id = r.local_id WHERE r.account_id = $1 AND r.kind = 'campaign_message'").bind(account_id).fetch_one(&state.db).await?;
    let data: Vec<Value> = rows
        .into_iter()
        .map(|(id, name, channel, status, sent_at, d)| json!({"id": id, "name": name, "channel": channel, "status": status, "sent_at": sent_at, "subject": d["subject"], "audience": d["audience"], "stats": d["stats"]}))
        .collect();
    Ok(json!({"data": data, "total": total}))
}

pub async fn mark(state: &AppState, account_id: Uuid, item: &str, done: bool) -> AppResult<Value> {
    if !report::MANUAL_ITEMS.contains(&item) {
        return Err(AppError::bad(format!("Only these items are ticked by hand: {}. The others are checked from your data.", report::MANUAL_ITEMS.join(", "))));
    }
    let imp = store::current(&state.db, account_id).await?;
    sqlx::query("UPDATE klaviyo_imports SET checklist = jsonb_set(checklist, ARRAY[$2], COALESCE(checklist->$2, '{}'::jsonb) || jsonb_build_object('done', $3::boolean), true), updated_at = now() WHERE id = $1")
        .bind(imp.id)
        .bind(item)
        .bind(done)
        .execute(&state.db)
        .await?;
    status_json(state, account_id).await
}

/// Look for Klaviyo's script on the account's website (fetched through the SSRF-safe path).
pub async fn check_site(state: &AppState, account_id: Uuid) -> AppResult<Value> {
    let imp = store::current(&state.db, account_id).await?;
    let website: Option<String> = sqlx::query_scalar("SELECT website FROM accounts WHERE id = $1").bind(account_id).fetch_one(&state.db).await?;
    let site = website.filter(|w| !w.trim().is_empty()).ok_or_else(|| AppError::bad("Add your website in Settings → Account first."))?;
    let url = if site.starts_with("http") { site.trim().to_string() } else { format!("https://{}", site.trim()) };
    let html = crate::flows::fetch_public_page(&url).await?.to_lowercase();
    let found = ["static.klaviyo.com", "klaviyo.com/onsite", "klaviyo.js", "a.klaviyo.com"].iter().any(|n| html.contains(n));
    sqlx::query("UPDATE klaviyo_imports SET checklist = jsonb_set(checklist, '{klaviyo_removed}', jsonb_build_object('done', $2::boolean, 'found', $3::boolean, 'checked_at', now()), true) WHERE id = $1")
        .bind(imp.id)
        .bind(!found)
        .bind(found)
        .execute(&state.db)
        .await?;
    status_json(state, account_id).await
}

// ---------- Jobs ----------

async fn run_survey(state: &AppState, id: Uuid) -> anyhow::Result<()> {
    let Some(imp) = store::load(&state.db, id).await? else { return Ok(()) };
    if imp.status != "surveying" {
        return Ok(());
    }
    let Some(key) = store::key(state, &imp) else {
        store::close(&state.db, id, "failed", Some("The Klaviyo key is no longer stored. Connect again.")).await?;
        return Ok(());
    };
    let tz: String = sqlx::query_scalar("SELECT timezone FROM accounts WHERE id = $1").bind(imp.account_id).fetch_one(&state.db).await?;
    let mut k = Client::new(state.http.clone(), &key);
    match survey::survey(&mut k, imp.events_since(), &tz).await {
        Ok(s) => {
            sqlx::query("UPDATE klaviyo_imports SET survey = $2, status = 'ready', error = NULL, updated_at = now() WHERE id = $1 AND status = 'surveying'").bind(id).bind(s).execute(&state.db).await?;
        }
        Err(KlaviyoError::RateLimited(secs)) => store::enqueue(&state.db, JOB_SURVEY, id, Some(Utc::now() + chrono::Duration::seconds(secs as i64))).await?,
        Err(e @ KlaviyoError::BadKey) => store::close(&state.db, id, "failed", Some(&e.to_string())).await?,
        Err(e) => {
            let n = imp.step_state["retries"].as_u64().unwrap_or(0) + 1;
            if n > 4 || !e.is_transient() {
                store::close(&state.db, id, "failed", Some(&format!("Couldn't read your Klaviyo account: {e}"))).await?;
            } else {
                sqlx::query("UPDATE klaviyo_imports SET step_state = jsonb_set(step_state, '{retries}', to_jsonb($2::bigint)) WHERE id = $1").bind(id).bind(n as i64).execute(&state.db).await?;
                store::enqueue(&state.db, JOB_SURVEY, id, Some(Utc::now() + chrono::Duration::seconds(30 * n as i64))).await?;
            }
        }
    }
    Ok(())
}

/// Every minute or so: start the hourly sync where due, and an extra one before a scheduled campaign
/// goes out so unsubscribes made in Klaviyo land first.
async fn start_due_syncs(state: &AppState) -> anyhow::Result<()> {
    let due: Vec<Uuid> = sqlx::query_scalar(
        "SELECT i.id FROM klaviyo_imports i
         WHERE i.sync_enabled AND i.status = 'done' AND i.api_key_enc IS NOT NULL
           AND (i.last_sync_at IS NULL OR i.last_sync_at < now() - interval '55 minutes'
                OR (i.last_sync_at < now() - interval '15 minutes' AND EXISTS (
                    SELECT 1 FROM campaigns c WHERE c.account_id = i.account_id AND c.status = 'scheduled' AND c.send_at < now() + interval '30 minutes')))
         LIMIT 200",
    )
    .fetch_all(&state.db)
    .await?;
    for id in due {
        runner::start_sync(&state.db, id).await?;
    }
    Ok(())
}

// ---------- HTTP ----------

#[derive(Deserialize)]
struct ConnectBody {
    api_key: String,
}

#[derive(Deserialize)]
struct SyncBody {
    enabled: bool,
}

#[derive(Deserialize)]
struct MarkBody {
    item: String,
    done: bool,
}

async fn status_h(State(s): State<AppState>, ctx: Ctx) -> AppResult<Json<Value>> {
    Ok(Json(status_json(&s, ctx.account_id).await?))
}
async fn connect_h(State(s): State<AppState>, ctx: Ctx, ApiJson(b): ApiJson<ConnectBody>) -> AppResult<Json<Value>> {
    Ok(Json(connect(&s, ctx.account_id, &b.api_key).await?))
}
async fn start_h(State(s): State<AppState>, ctx: Ctx, ApiJson(b): ApiJson<StartBody>) -> AppResult<Json<Value>> {
    Ok(Json(start(&s, ctx.account_id, b).await?))
}
async fn pause_h(State(s): State<AppState>, ctx: Ctx) -> AppResult<Json<Value>> {
    Ok(Json(pause(&s, ctx.account_id, false).await?))
}
async fn resume_h(State(s): State<AppState>, ctx: Ctx) -> AppResult<Json<Value>> {
    Ok(Json(pause(&s, ctx.account_id, true).await?))
}
async fn cancel_h(State(s): State<AppState>, ctx: Ctx) -> AppResult<Json<Value>> {
    Ok(Json(cancel(&s, ctx.account_id).await?))
}
async fn sync_h(State(s): State<AppState>, ctx: Ctx, ApiJson(b): ApiJson<SyncBody>) -> AppResult<Json<Value>> {
    Ok(Json(set_sync(&s, ctx.account_id, b.enabled).await?))
}
async fn sync_now_h(State(s): State<AppState>, ctx: Ctx) -> AppResult<Json<Value>> {
    Ok(Json(sync_now(&s, ctx.account_id).await?))
}
async fn report_h(State(s): State<AppState>, ctx: Ctx) -> AppResult<Json<Value>> {
    Ok(Json(comparison(&s, ctx.account_id).await?))
}
async fn report_csv_h(State(s): State<AppState>, ctx: Ctx) -> AppResult<Response> {
    let r = comparison(&s, ctx.account_id).await?;
    let body = report::csv(&r);
    Ok(([(header::CONTENT_TYPE, "text/csv; charset=utf-8"), (header::CONTENT_DISPOSITION, "attachment; filename=\"klaviyo-comparison.csv\"")], body).into_response())
}
async fn issues_h(State(s): State<AppState>, ctx: Ctx, Query(q): Query<IssueQuery>) -> AppResult<Json<Value>> {
    Ok(Json(issues(&s, ctx.account_id, &q).await?))
}
async fn campaigns_h(State(s): State<AppState>, ctx: Ctx, Query(q): Query<IssueQuery>) -> AppResult<Json<Value>> {
    Ok(Json(campaign_history(&s, ctx.account_id, &q).await?))
}
async fn mark_h(State(s): State<AppState>, ctx: Ctx, ApiJson(b): ApiJson<MarkBody>) -> AppResult<Json<Value>> {
    Ok(Json(mark(&s, ctx.account_id, &b.item, b.done).await?))
}
async fn check_site_h(State(s): State<AppState>, ctx: Ctx) -> AppResult<Json<Value>> {
    Ok(Json(check_site(&s, ctx.account_id).await?))
}

// ---------- MCP ----------

fn args_start(a: &Value) -> AppResult<StartBody> {
    serde_json::from_value(a.clone()).map_err(|e| AppError::bad(format!("invalid arguments: {e}")))
}

#[async_trait]
impl Feature for Module {
    fn name(&self) -> &'static str {
        "klaviyo_import"
    }

    fn routes(&self) -> Router<AppState> {
        Router::new()
            .route("/api/klaviyo", get(status_h))
            .route("/api/klaviyo/connect", post(connect_h).layer(axum::extract::DefaultBodyLimit::max(4 * 1024)))
            .route("/api/klaviyo/start", post(start_h).layer(axum::extract::DefaultBodyLimit::max(32 * 1024)))
            .route("/api/klaviyo/pause", post(pause_h))
            .route("/api/klaviyo/resume", post(resume_h))
            .route("/api/klaviyo/cancel", post(cancel_h))
            .route("/api/klaviyo/sync", post(sync_h))
            .route("/api/klaviyo/sync-now", post(sync_now_h))
            .route("/api/klaviyo/report", get(report_h))
            .route("/api/klaviyo/report.csv", get(report_csv_h))
            .route("/api/klaviyo/issues", get(issues_h))
            .route("/api/klaviyo/campaigns", get(campaigns_h))
            .route("/api/klaviyo/checklist", post(mark_h))
            .route("/api/klaviyo/checklist/check-site", post(check_site_h))
    }

    fn mcp_tools(&self) -> Vec<Value> {
        let what = json!({"type": "array", "maxItems": 7, "items": {"enum": CHOOSABLE}, "description": "Parts to import besides profiles and consent (always imported). Default all."});
        vec![
            tool("klaviyo_survey", "Connect Klaviyo and survey it",
                "With api_key: check a Klaviyo private API key (read-only scopes: Accounts, Campaigns, Catalogs, Coupons, Events, Flows, Forms, Lists, Metrics, Profiles, Segments, Templates) and start counting what's in the account. Without it: return the survey of the current import (counts of profiles, lists, segments, metrics, estimated events, templates, campaigns, flows, forms, catalog items, coupons, suppressions, estimated minutes and missing scopes). Nothing is imported. The key is stored encrypted only while the import runs (and while the ongoing sync is on).",
                obj(json!({"api_key": {"type": "string", "maxLength": 200, "description": "Klaviyo private API key (pk_...)."}}), &[]), false, false),
            tool("klaviyo_start_import", "Start the Klaviyo import",
                "Import from Klaviyo in the background: profiles with their exact email/SMS consent and suppressions, then lists, segments (as rules where they translate, otherwise as a snapshot list), templates, campaigns (history and drafts), flows (always as drafts), forms (name and list) and event history. Nobody is messaged and no flow starts. Without confirm=true returns a dry run. Track it with klaviyo_import_status.",
                obj(json!({"confirm": {"type": "boolean"}, "what": what, "events_from": {"type": "string", "description": "First day of events to import, YYYY-MM-DD, or \"all\". Default two years ago."},
                    "metrics": {"type": "array", "maxItems": 200, "items": {"type": "string", "maxLength": 128}, "description": "Klaviyo metric names or ids to import events for. Default: orders, checkouts, product views, carts and email/SMS engagement."},
                    "sync": {"type": "boolean", "description": "Keep syncing changes from Klaviyo every hour after the import (keeps the key stored until turned off)."}}), &[]), false, false),
            tool("klaviyo_import_status", "Klaviyo import status", "Status, current step, progress per step, issue counts, the ongoing sync and the switch-over checklist.", obj(json!({}), &[]), true, false),
            tool("klaviyo_pause_import", "Pause or resume the Klaviyo import", "Pause a running import, or resume a paused one (resume=true). It picks up where it stopped.", obj(json!({"resume": {"type": "boolean"}}), &[]), false, false),
            tool("klaviyo_cancel_import", "Cancel the Klaviyo import", "Stop the import (or the ongoing sync) and delete the stored Klaviyo key. What was already imported stays.", obj(json!({}), &[]), false, false),
            tool("klaviyo_comparison_report", "Klaviyo comparison report",
                "Klaviyo's numbers next to DumbMail's after the import: profiles, email and SMS consent (subscribed, unsubscribed, never subscribed, suppressed, reachable), list sizes, segment sizes, event counts per metric and revenue per month, each marked ok or not with a note. Also counts what didn't come across exactly (see klaviyo_import_issues).",
                obj(json!({}), &[]), true, false),
            tool("klaviyo_import_issues", "What the Klaviyo import couldn't bring across", "Segments imported as snapshots and why, template tags removed, flow steps skipped, profiles skipped, and notes.",
                obj(json!({"step": {"type": "string"}, "limit": {"type": "integer", "minimum": 1, "maximum": 500}, "offset": {"type": "integer", "minimum": 0}}), &[]), true, false),
            tool("klaviyo_set_sync", "Turn the ongoing Klaviyo sync on or off",
                "While on, new profiles, consent changes (unsubscribes first), list joins and events are pulled from Klaviyo every hour, and before a scheduled campaign. Turning it off after the import deletes the stored key.",
                obj(json!({"enabled": {"type": "boolean"}}), &["enabled"]), false, false),
            tool("klaviyo_sync_now", "Sync from Klaviyo now", "Pull changes from Klaviyo right away, e.g. just before sending a campaign so recent Klaviyo unsubscribes are honored. Needs the ongoing sync on.", obj(json!({}), &[]), false, false),
            tool("klaviyo_checklist", "Switch-over checklist",
                "The steps to finish moving from Klaviyo, most checked from live data. item+done ticks one of the hand-checked items (klaviyo_removed, flows_reviewed, warmup); check_site=true looks for Klaviyo's script on the account's website.",
                obj(json!({"item": {"enum": report::MANUAL_ITEMS}, "done": {"type": "boolean"}, "check_site": {"type": "boolean"}}), &[]), false, false),
        ]
    }

    async fn mcp_call(&self, state: &AppState, ctx: &Ctx, name: &str, a: &Value) -> Option<AppResult<Value>> {
        let acct = ctx.account_id;
        Some(match name {
            "klaviyo_survey" => match a["api_key"].as_str() {
                Some(k) => connect(state, acct, k).await,
                None => status_json(state, acct).await,
            },
            "klaviyo_start_import" => match args_start(a) {
                Ok(b) => start(state, acct, b).await,
                Err(e) => Err(e),
            },
            "klaviyo_import_status" => status_json(state, acct).await,
            "klaviyo_pause_import" => pause(state, acct, a["resume"].as_bool().unwrap_or(false)).await,
            "klaviyo_cancel_import" => cancel(state, acct).await,
            "klaviyo_comparison_report" => comparison(state, acct).await,
            "klaviyo_import_issues" => {
                let q = IssueQuery { step: a["step"].as_str().map(String::from), limit: a["limit"].as_i64(), offset: a["offset"].as_i64() };
                issues(state, acct, &q).await
            }
            "klaviyo_set_sync" => match a["enabled"].as_bool() {
                Some(e) => set_sync(state, acct, e).await,
                None => Err(AppError::bad("enabled is required")),
            },
            "klaviyo_sync_now" => sync_now(state, acct).await,
            "klaviyo_checklist" => {
                if a["check_site"].as_bool() == Some(true) {
                    check_site(state, acct).await
                } else if let Some(item) = a["item"].as_str() {
                    mark(state, acct, item, a["done"].as_bool().unwrap_or(true)).await
                } else {
                    status_json(state, acct).await.map(|v| v["checklist"].clone())
                }
            }
            _ => return None,
        })
    }

    fn mcp_docs(&self) -> Vec<McpDoc> {
        vec![McpDoc { slug: "moving-from-klaviyo", name: "Moving from Klaviyo", description: "Step-by-step guide to importing a Klaviyo account and switching over", text: DOC }]
    }

    async fn run_job(&self, state: &AppState, kind: &str, payload: &Value) -> Option<anyhow::Result<()>> {
        let id = || payload["import_id"].as_str().and_then(|s| s.parse::<Uuid>().ok()).ok_or_else(|| anyhow::anyhow!("job payload missing import_id"));
        match kind {
            JOB_RUN => Some(match id() {
                Ok(i) => runner::run(state, i).await,
                Err(e) => Err(e),
            }),
            JOB_SURVEY => Some(match id() {
                Ok(i) => run_survey(state, i).await,
                Err(e) => Err(e),
            }),
            _ => None,
        }
    }

    async fn tick(&self, state: &AppState, tick: u64) -> anyhow::Result<()> {
        if tick.is_multiple_of(3) {
            start_due_syncs(state).await?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn tools_are_well_formed() {
        use super::Feature;
        let t = super::Module.mcp_tools();
        assert_eq!(t.len(), 10);
        for x in &t {
            assert_eq!(x["inputSchema"]["type"], "object");
            assert!(x["name"].as_str().unwrap().starts_with("klaviyo_"));
        }
        assert!(super::DOC.contains("Klaviyo"));
    }
}
