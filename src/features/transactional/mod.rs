//! Transactional email API: receipts, password resets, shipping notices and other mail a person
//! triggered. It skips marketing consent but never hard-bounced or complaining addresses, has no
//! unsubscribe footer, isn't held by the warm-up limit, is billed like any email, and is kept
//! honest by being off until the owner turns it on, a restricted mode on the hosted service, content
//! rules, caps and an automatic pause (see `review` and `guard`).

pub mod guard;
pub mod review;
pub mod send;

use async_trait::async_trait;
use axum::extract::{Path, Query, State};
use axum::http::HeaderMap;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};
use uuid::Uuid;

use super::{Feature, McpDoc};
use crate::api::ApiJson;
use crate::auth::Ctx;
use crate::error::{AppError, AppResult};
use crate::mcp::{obj, tool};
use crate::state::AppState;

pub struct Module;

pub const RETRY_JOB: &str = "transactional_retry";

/// Built in the background after startup (see `reports::indexes`), since `messages` is big: hard
/// bounces and complaints by address, for the suppression check before every send. Sends are
/// correct without it, only slower.
pub const INDEXES: &[(&str, &str)] =
    &[("messages_bad_address_idx", "CREATE INDEX CONCURRENTLY messages_bad_address_idx ON messages (account_id, to_address) WHERE status IN ('bounced', 'complained')")];
const INDEX_LOCK_KEY: i64 = 0x0d4d_7472_616e_7378;
const DOC: &str = include_str!("../../../docs/TRANSACTIONAL.md");

async fn send_h(State(state): State<AppState>, ctx: Ctx, ApiJson(b): ApiJson<send::SendBody>) -> AppResult<Json<Value>> {
    let (mut v, replayed) = send::send(&state, &ctx, b, if ctx.via_api_key { "api" } else { "dashboard" }).await?;
    v["replayed"] = json!(replayed);
    Ok(Json(v))
}

#[derive(Deserialize)]
struct ListQuery {
    q: Option<String>,
    status: Option<String>,
    limit: Option<i64>,
    offset: Option<i64>,
}

const STATUSES: &[&str] = &["queued", "sending", "sent", "delivered", "opened", "clicked", "bounced", "complained", "skipped", "failed", "unknown"];

async fn list(state: &AppState, account_id: Uuid, q: ListQuery) -> AppResult<Value> {
    if let Some(s) = q.status.as_deref().filter(|s| !STATUSES.contains(s)) {
        return Err(AppError::bad(format!("status must be one of {}, not {s:?}", STATUSES.join(", "))));
    }
    let search = q.q.as_deref().map(|s| s.chars().take(200).collect::<String>());
    let limit = q.limit.unwrap_or(50).clamp(1, 200);
    let offset = q.offset.unwrap_or(0).clamp(0, 100_000);
    let rows = send::list_rows(&state.db, account_id, None, search.as_deref(), q.status.as_deref(), limit, offset).await?;
    Ok(json!({"data": rows, "limit": limit, "offset": offset}))
}

async fn list_h(State(state): State<AppState>, ctx: Ctx, Query(q): Query<ListQuery>) -> AppResult<Json<Value>> {
    Ok(Json(list(&state, ctx.account_id, q).await?))
}

async fn get_h(State(state): State<AppState>, ctx: Ctx, Path(id): Path<Uuid>) -> AppResult<Json<Value>> {
    Ok(Json(send::get(&state.db, ctx.account_id, id).await?))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TemplateBody {
    name: String,
    content: Value,
}

fn template_json(id: Uuid, name: &str, content: &Value, created: chrono::DateTime<chrono::Utc>, updated: chrono::DateTime<chrono::Utc>) -> Value {
    json!({"id": id, "name": name, "channel": "email", "transactional": true, "content": content, "created_at": created, "updated_at": updated, "review": review::review(content)})
}

/// Create a transactional template (an email template the transactional API may send). Content that
/// reads like marketing is refused; edits are checked again when saved, and at every send.
async fn create_template(state: &AppState, account_id: Uuid, b: TemplateBody) -> AppResult<Value> {
    let name = b.name.trim();
    if name.is_empty() || name.chars().count() > 200 {
        return Err(AppError::bad("give the template a name (at most 200 characters)"));
    }
    check_template_content(&b.content)?;
    let row: (Uuid, chrono::DateTime<chrono::Utc>, chrono::DateTime<chrono::Utc>) = sqlx::query_as(
        "INSERT INTO templates (id, account_id, name, channel, content, transactional) VALUES ($1, $2, $3, 'email', $4, true) RETURNING id, created_at, updated_at",
    )
    .bind(Uuid::now_v7())
    .bind(account_id)
    .bind(name)
    .bind(&b.content)
    .fetch_one(&state.db)
    .await?;
    Ok(template_json(row.0, name, &b.content, row.1, row.2))
}

/// Editing a transactional template (through `PATCH /api/templates/{id}`): its new content passes the
/// same check as a new one, or the edit is refused. Other templates aren't affected.
pub async fn check_template_edit(db: &sqlx::PgPool, account_id: Uuid, id: Uuid, content: Option<&Value>) -> AppResult<()> {
    let Some(content) = content else { return Ok(()) };
    let transactional: Option<bool> = sqlx::query_scalar("SELECT transactional FROM templates WHERE id = $1 AND account_id = $2").bind(id).bind(account_id).fetch_optional(db).await?;
    if transactional != Some(true) {
        return Ok(());
    }
    check_template_content(content)
}

fn check_template_content(content: &Value) -> AppResult<()> {
    if !content.is_object() || serde_json::to_string(content)?.len() > 600 * 1024 {
        return Err(AppError::bad("content must be an email content object (at most 600 KB)"));
    }
    if let Some(p) = crate::features::content::prepare::transactional_problem(content) {
        return Err(AppError::bad(p));
    }
    let r = review::review(content);
    if r.blocked {
        return Err(AppError::bad(review::blocked_message(&r)));
    }
    Ok(())
}

async fn list_templates(state: &AppState, account_id: Uuid) -> AppResult<Value> {
    let rows: Vec<(Uuid, String, Value, chrono::DateTime<chrono::Utc>, chrono::DateTime<chrono::Utc>)> = sqlx::query_as(
        "SELECT id, name, content, created_at, updated_at FROM templates WHERE account_id = $1 AND transactional AND channel = 'email' ORDER BY updated_at DESC LIMIT 500",
    )
    .bind(account_id)
    .fetch_all(&state.db)
    .await?;
    Ok(json!({"data": rows.iter().map(|(id, name, c, cr, up)| template_json(*id, name, c, *cr, *up)).collect::<Vec<_>>()}))
}

async fn templates_h(State(state): State<AppState>, ctx: Ctx) -> AppResult<Json<Value>> {
    Ok(Json(list_templates(&state, ctx.account_id).await?))
}

async fn create_template_h(State(state): State<AppState>, ctx: Ctx, ApiJson(b): ApiJson<TemplateBody>) -> AppResult<Json<Value>> {
    Ok(Json(create_template(&state, ctx.account_id, b).await?))
}

#[derive(Deserialize)]
struct ReviewBody {
    content: Value,
}

async fn review_h(_ctx: Ctx, ApiJson(b): ApiJson<ReviewBody>) -> AppResult<Json<Value>> {
    Ok(Json(serde_json::to_value(review::review(&b.content))?))
}

async fn settings_h(State(state): State<AppState>, ctx: Ctx) -> AppResult<Json<Value>> {
    Ok(Json(guard::settings_json(&state, ctx.account_id).await?))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EnableBody {
    enabled: bool,
    #[serde(default)]
    acknowledge: bool,
}

/// Owner: turn transactional email on (accepting what it's for) or off.
async fn update_settings_h(State(state): State<AppState>, ctx: Ctx, ApiJson(b): ApiJson<EnableBody>) -> AppResult<Json<Value>> {
    Ok(Json(guard::set_enabled(&state, &ctx, b.enabled, b.acknowledge).await?))
}

async fn admin_account(state: &AppState, b: &Value) -> AppResult<Uuid> {
    let id: Uuid = b["account_id"].as_str().and_then(|s| s.trim().parse().ok()).ok_or_else(|| AppError::bad("account_id must be a UUID"))?;
    let found: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM accounts WHERE id = $1)").bind(id).fetch_one(&state.db).await?;
    if found { Ok(id) } else { Err(AppError::not_found("account")) }
}

/// Operator: lift an automatic transactional pause. Bearer ADMIN_TOKEN.
async fn admin_unpause_h(State(state): State<AppState>, headers: HeaderMap, Json(b): Json<Value>) -> AppResult<Json<Value>> {
    crate::public::check_admin(&state, &headers)?;
    let id = admin_account(&state, &b).await?;
    let was = guard::unpause(&state.db, id).await?;
    tracing::warn!(account = %id, was_paused = was, "admin lifted the transactional pause");
    Ok(Json(json!({"account_id": id, "was_paused": was, "paused": false})))
}

/// Operator: set an account's transactional caps (null returns a cap to its default). Bearer ADMIN_TOKEN.
async fn admin_limits_h(State(state): State<AppState>, headers: HeaderMap, Json(b): Json<Value>) -> AppResult<Json<Value>> {
    crate::public::check_admin(&state, &headers)?;
    let id = admin_account(&state, &b).await?;
    let num = |k: &str, max: i64| -> AppResult<Option<i32>> {
        match &b[k] {
            Value::Null => Ok(None),
            v => v.as_i64().filter(|n| (0..=max).contains(n)).map(|n| Some(n as i32)).ok_or_else(|| AppError::bad(format!("{k} must be a whole number from 0 to {max}, or null for the default"))),
        }
    };
    guard::set_caps(&state.db, id, num("per_minute_limit", 100_000)?, num("daily_limit", 10_000_000)?).await?;
    tracing::warn!(account = %id, "admin set transactional caps");
    Ok(Json(guard::settings_json(&state, id).await?))
}

/// Operator: grant full mode (inline content and the normal caps) or put an account back in
/// restricted mode. Bearer ADMIN_TOKEN.
async fn admin_mode_h(State(state): State<AppState>, headers: HeaderMap, Json(b): Json<Value>) -> AppResult<Json<Value>> {
    crate::public::check_admin(&state, &headers)?;
    let id = admin_account(&state, &b).await?;
    let full = match b["mode"].as_str() {
        Some("full") => true,
        Some("restricted") => false,
        _ => return Err(AppError::bad("mode must be full or restricted")),
    };
    guard::set_full_mode(&state.db, id, full).await?;
    tracing::warn!(account = %id, full, "admin set transactional mode");
    Ok(Json(guard::settings_json(&state, id).await?))
}

fn body_from(a: &Value) -> AppResult<send::SendBody> {
    let mut a = a.clone();
    if let Some(o) = a.as_object_mut() {
        o.remove("confirm");
        o.remove("include_html");
    }
    crate::mcp::from(a)
}

fn uuid_arg(a: &Value, k: &str) -> AppResult<Uuid> {
    a[k].as_str().and_then(|s| s.parse().ok()).ok_or_else(|| AppError::bad(format!("`{k}` must be an id")))
}

#[async_trait]
impl Feature for Module {
    fn name(&self) -> &'static str {
        "transactional"
    }

    fn routes(&self) -> Router<AppState> {
        Router::new()
            .route("/api/transactional/send", post(send_h).layer(axum::extract::DefaultBodyLimit::max(1024 * 1024)))
            .route("/api/transactional/sends", get(list_h))
            .route("/api/transactional/sends/{id}", get(get_h))
            .route("/api/transactional/templates", get(templates_h).post(create_template_h).layer(axum::extract::DefaultBodyLimit::max(1024 * 1024)))
            .route("/api/transactional/review", post(review_h).layer(axum::extract::DefaultBodyLimit::max(1024 * 1024)))
            .route("/api/transactional/settings", get(settings_h).patch(update_settings_h))
            .route("/admin/transactional/unpause", post(admin_unpause_h))
            .route("/admin/transactional/limits", post(admin_limits_h))
            .route("/admin/transactional/mode", post(admin_mode_h))
    }

    fn mcp_tools(&self) -> Vec<Value> {
        let to = json!({"description": "One recipient: an email address, or {email, name?, first_name?, last_name?, external_id?, properties?}.", "oneOf": [{"type": "string"}, {"type": "object", "properties": {"email": {"type": "string"}, "name": {"type": "string"}, "first_name": {"type": "string"}, "last_name": {"type": "string"}, "external_id": {"type": "string"}, "properties": {"type": "object"}}, "required": ["email"], "additionalProperties": false}]});
        let content = json!({"type": "object", "description": "Inline content instead of template_id: {subject, preheader?, blocks? (same block format as campaigns) | html? | text?}. Variables fill {{ name }} tags; profile fields ({{ first_name }}) work too."});
        let send_props = json!({
            "to": to, "template_id": {"type": "string", "description": "A transactional template (create_transactional_template)."}, "content": content,
            "subject": {"type": "string", "maxLength": 998}, "variables": {"type": "object", "description": "Values for {{ tags }}, e.g. {order_id: '1042', items: [...]}. At most 64 KB."},
            "from_email": {"type": "string", "description": "Must be on a verified sending domain. Defaults to the account's sender."}, "from_name": {"type": "string"}, "reply_to": {"type": "string"},
            "idempotency_key": {"type": "string", "maxLength": 255, "description": "The same key within 24 hours returns the first result without sending again."},
            "metadata": {"type": "object", "description": "Your own fields, stored with the send and on the profile event (at most 50 keys, 8 KB)."},
            "track_clicks": {"type": "boolean", "description": "Rewrite links for click tracking. Default false."},
            "track_opens": {"type": "boolean", "description": "Add the open pixel. Default true."},
            "address_footer": {"type": "boolean", "description": "Add the account's name and postal address at the bottom. Default false."},
            "include_html": {"type": "boolean", "description": "Dry run only: include the rendered HTML."},
            "confirm": {"type": "boolean"}
        });
        vec![
            tool("send_transactional_email", "Send a transactional email", "Send one email a person triggered (receipt, password reset, shipping notice) to one recipient. It skips marketing consent, so it must not promote anything: content that reads like marketing is refused. Transactional email is off until the account owner turns it on in Settings; on the hosted service it starts in restricted mode (saved transactional templates only, 200 a day) until full mode is granted. Hard-bounced and complaining addresses are never sent to (the result has status skipped). Without confirm=true it renders the email and reports whether it would send, without sending. With confirm=true it sends to a real person; get the user's go-ahead first. To send again, make a new call with fresh variables.",
                obj(send_props, &["to"]), false, true),
            tool("list_transactional_sends", "List transactional sends", "Recent transactional emails, newest first, with status (queued, sending, sent, delivered, opened, clicked, bounced, complained, skipped, failed, or unknown: cut off mid-send, may have gone out, never sent again). q searches recipient and subject. Variables and email bodies are never stored, so they aren't shown.",
                obj(json!({"q": {"type": "string"}, "status": {"enum": STATUSES}, "limit": {"type": "integer", "minimum": 1, "maximum": 200}, "offset": {"type": "integer", "minimum": 0}}), &[]), true, false),
            tool("get_transactional_send", "Get a transactional send", "One transactional send: recipient, subject, template, status, attempts, delivery and engagement times and metadata (never its variables or body).",
                obj(json!({"id": {"type": "string"}}), &["id"]), true, false),
            tool("create_transactional_template", "Create a transactional template", "Save an email template the transactional API can send by template_id. Content that reads like marketing is refused: discount or offer language (20% off, $10 off, promo code, limited time, sale), shop now / buy now calls to action, links to 3 or more product pages, or an unsubscribe link.",
                obj(json!({"name": {"type": "string"}, "content": content}), &["name", "content"]), false, false),
            tool("list_transactional_templates", "List transactional templates", "Templates the transactional API can send, with any review warnings.", obj(json!({}), &[]), true, false),
            tool("get_transactional_settings", "Transactional status and limits", "Whether transactional email is on and its mode (off, restricted: saved templates only and a low daily cap, or full), the caps per minute and per 24 hours, how much of each is used, and whether it's paused. Only the account owner can turn it on, in the dashboard.", obj(json!({}), &[]), true, false),
        ]
    }

    async fn mcp_call(&self, state: &AppState, ctx: &Ctx, name: &str, a: &Value) -> Option<AppResult<Value>> {
        let acct = ctx.account_id;
        let r = match name {
            "send_transactional_email" => async {
                let b = body_from(a)?;
                if a["confirm"].as_bool() != Some(true) {
                    return send::dry_run(state, ctx, b, a["include_html"].as_bool().unwrap_or(false)).await;
                }
                let (mut v, replayed) = send::send(state, ctx, b, "mcp").await?;
                v["replayed"] = json!(replayed);
                Ok(v)
            }
            .await,
            "list_transactional_sends" => list(state, acct, ListQuery { q: a["q"].as_str().map(String::from), status: a["status"].as_str().map(String::from), limit: a["limit"].as_i64(), offset: a["offset"].as_i64() }).await,
            "get_transactional_send" => async { send::get(&state.db, acct, uuid_arg(a, "id")?).await }.await,
            "create_transactional_template" => async { create_template(state, acct, crate::mcp::from(a.clone())?).await }.await,
            "list_transactional_templates" => list_templates(state, acct).await,
            "get_transactional_settings" => guard::settings_json(state, acct).await,
            _ => return None,
        };
        Some(r)
    }

    fn mcp_docs(&self) -> Vec<McpDoc> {
        vec![McpDoc { slug: "transactional", name: "Transactional email API", description: "Sending receipts, password resets and other triggered email: request fields, idempotency, suppression, limits and examples", text: DOC }]
    }

    async fn run_job(&self, state: &AppState, kind: &str, payload: &Value) -> Option<anyhow::Result<()>> {
        if kind != RETRY_JOB {
            return None;
        }
        let id = payload["send_id"].as_str().and_then(|s| s.parse::<Uuid>().ok());
        Some(match id {
            Some(id) => send::retry(state, id).await,
            None => Ok(()),
        })
    }

    async fn after_start(&self, state: &AppState) -> anyhow::Result<()> {
        crate::features::reports::indexes::build(state, INDEX_LOCK_KEY, "transactional", INDEXES).await
    }

    async fn tick(&self, state: &AppState, tick: u64) -> anyhow::Result<()> {
        send::sweep(&state.db).await?;
        if tick.is_multiple_of(3) {
            guard::check_recent(state).await?;
        }
        Ok(())
    }
}
