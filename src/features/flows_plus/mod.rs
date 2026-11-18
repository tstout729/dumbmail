//! Flow tools people moving from Klaviyo expect: delays until a time of day, a weekday or a date
//! (`delay`), trigger splits on the event that started the run (`split`), notification steps
//! (`notify`), adding people who already qualify (`backfill`), and per-step results with the list
//! of people waiting at each step (`results`). The engine changes are small hooks in `src/flows.rs`;
//! the work lives here. See docs/FLOWS_PLUS.md.

pub mod backfill;
pub mod delay;
pub mod notify;
pub mod results;
pub mod split;

use async_trait::async_trait;
use axum::extract::{Path, Query, State};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Form, Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};
use uuid::Uuid;

use super::team::access::{public, rule, Access, Perm, RouteRule, ToolRule};
use super::{Feature, McpDoc};
use crate::api::ApiJson;
use crate::auth::Ctx;
use crate::error::{AppError, AppResult};
use crate::flows::FlowDef;
use crate::mcp::{obj, tool};
use crate::state::AppState;

pub struct Module;

const DOC: &str = include_str!("../../../docs/FLOWS_PLUS.md");

static ROUTES: &[RouteRule] = &[
    rule("GET", "/api/flows/{id}/results", Perm::MessagingRead),
    rule("GET", "/api/flows/{id}/steps/{node_id}/waiting", Perm::ProfilesRead),
    rule("POST", "/api/flows/{id}/runs/remove", Perm::FlowLive),
    rule("POST", "/api/flows/{id}/backfill", Perm::FlowLive),
    rule("GET", "/api/flows/{id}/backfills", Perm::MessagingRead),
    rule("POST", "/api/flows/{id}/backfills/{backfill_id}/cancel", Perm::FlowLive),
    rule("GET", "/api/flows-plus/notify-addresses", Perm::FlowEdit),
    rule("POST", "/api/flows-plus/notify-addresses", Perm::FlowEdit),
    rule("POST", "/api/flows-plus/notify-addresses/remove", Perm::FlowEdit),
    // Emailing an outside address is a change to who the account contacts: account settings.
    rule("POST", "/api/flows-plus/notify-addresses/send-confirmation", Perm::SettingsWrite),
    // Clearing a bounce is a deliverability decision: Admins and Owners only.
    rule("POST", "/api/flows-plus/notify-addresses/clear-bounce", Perm::Domains),
    rule("GET", "/api/flows-plus/metric-trust", Perm::AudienceRead),
    public("GET", "/flow-notify/confirm"),
    public("POST", "/flow-notify/confirm"),
];

static TOOLS: &[ToolRule] = &[
    ToolRule { name: "get_flow_results", perm: Perm::MessagingRead },
    ToolRule { name: "list_flow_waiting", perm: Perm::ProfilesRead },
    ToolRule { name: "remove_from_flow", perm: Perm::FlowLive },
    ToolRule { name: "backfill_flow", perm: Perm::FlowLive },
    ToolRule { name: "list_flow_backfills", perm: Perm::MessagingRead },
    ToolRule { name: "cancel_flow_backfill", perm: Perm::FlowLive },
    ToolRule { name: "list_notify_addresses", perm: Perm::FlowEdit },
    ToolRule { name: "add_notify_address", perm: Perm::FlowEdit },
    ToolRule { name: "remove_notify_address", perm: Perm::FlowEdit },
    ToolRule { name: "send_notify_confirmation", perm: Perm::SettingsWrite },
    ToolRule { name: "clear_notify_bounce", perm: Perm::Domains },
];

/// Check this module's step options when a flow is saved (called from `flows::parse_definition`).
pub fn validate(def: &FlowDef, raw: &Value) -> AppResult<()> {
    for n in &def.nodes {
        match n.kind.as_str() {
            "delay" => delay::validate(n)?,
            "condition" => split::validate(n)?,
            "notify" => notify::validate(n)?,
            _ => {}
        }
    }
    notify::validate_settings(raw)
}

/// When a flow is saved (called from the flow API's step checks): outside notification addresses
/// are recorded as unconfirmed. Nothing is emailed.
pub async fn on_save(state: &AppState, account_id: Uuid, def: &FlowDef) -> AppResult<()> {
    notify::on_save(state, account_id, def).await
}

/// Whether the website script may send `metric` (so its values can't be trusted).
pub fn browser_can_send(metric: &str) -> bool {
    !crate::audience::is_reserved_metric(metric)
}

// ---------- HTTP ----------

async fn results_h(State(state): State<AppState>, ctx: Ctx, Path(id): Path<Uuid>, Query(q): Query<results::Range>) -> AppResult<Json<Value>> {
    Ok(Json(results::results(&state, ctx.account_id, id, q).await?))
}

async fn waiting_h(State(state): State<AppState>, ctx: Ctx, Path((id, node_id)): Path<(Uuid, String)>, Query(q): Query<results::WaitingQuery>) -> AppResult<Json<Value>> {
    Ok(Json(results::waiting(&state, ctx.account_id, id, &node_id, q).await?))
}

async fn remove_h(State(state): State<AppState>, ctx: Ctx, Path(id): Path<Uuid>, ApiJson(b): ApiJson<results::RemoveBody>) -> AppResult<Json<Value>> {
    Ok(Json(results::remove(&state, &ctx, id, b).await?))
}

#[derive(Deserialize, Default)]
struct BackfillBody {
    #[serde(default)]
    confirm: bool,
    limit: Option<i64>,
}

async fn backfill_h(State(state): State<AppState>, ctx: Ctx, Path(id): Path<Uuid>, ApiJson(b): ApiJson<BackfillBody>) -> AppResult<Json<Value>> {
    Ok(Json(backfill::start(&state, &ctx, id, b.confirm, b.limit).await?))
}

async fn backfills_h(State(state): State<AppState>, ctx: Ctx, Path(id): Path<Uuid>) -> AppResult<Json<Value>> {
    Ok(Json(json!({"data": backfill::list(&state, ctx.account_id, id).await?})))
}

async fn cancel_h(State(state): State<AppState>, ctx: Ctx, Path((id, bid)): Path<(Uuid, Uuid)>) -> AppResult<Json<Value>> {
    Ok(Json(backfill::cancel(&state, &ctx, id, bid).await?))
}

#[derive(Deserialize, Default)]
struct AddressQuery {
    /// Comma-separated addresses to give the status of.
    emails: Option<String>,
}

async fn addresses_json(state: &AppState, account_id: Uuid, emails: Option<&str>) -> AppResult<Value> {
    let asked: Vec<String> = emails.unwrap_or("").split(',').map(str::trim).filter(|s| !s.is_empty()).map(str::to_string).collect();
    Ok(json!({"data": notify::list_addresses(&state.db, account_id).await?, "statuses": notify::statuses(&state.db, account_id, &asked).await?}))
}

async fn addresses_h(State(state): State<AppState>, ctx: Ctx, Query(q): Query<AddressQuery>) -> AppResult<Json<Value>> {
    Ok(Json(addresses_json(&state, ctx.account_id, q.emails.as_deref()).await?))
}

#[derive(Deserialize)]
struct AddressBody {
    email: String,
}

async fn add_address(state: &AppState, ctx: &Ctx, email: &str) -> AppResult<Value> {
    let e = crate::models::normalize_email(email).ok_or_else(|| AppError::bad("that isn't an email address"))?;
    let out = notify::record_addresses(state, ctx.account_id, std::slice::from_ref(&e)).await?;
    Ok(out.into_iter().next().unwrap_or(json!({"email": e})))
}

#[derive(Deserialize)]
struct ConfirmBody {
    email: String,
    #[serde(default)]
    confirm: bool,
}

async fn send_confirmation(state: &AppState, ctx: &Ctx, email: &str, confirm: bool) -> AppResult<Value> {
    let v = notify::send_confirmation_request(state, ctx.account_id, email, confirm).await?;
    if v["sent"] == true {
        crate::features::team::audit(state, ctx, "flow.notify_confirmation_sent", v["email"].as_str(), json!({})).await;
    }
    Ok(v)
}

async fn send_confirmation_h(State(state): State<AppState>, ctx: Ctx, ApiJson(b): ApiJson<ConfirmBody>) -> AppResult<Json<Value>> {
    Ok(Json(send_confirmation(&state, &ctx, &b.email, b.confirm).await?))
}

async fn clear_bounce(state: &AppState, ctx: &Ctx, email: &str) -> AppResult<Value> {
    let cleared = notify::clear_bounce(&state.db, ctx.account_id, email).await?;
    if cleared {
        crate::features::team::audit(state, ctx, "flow.notify_bounce_clear", Some(&email.trim().to_lowercase()), json!({})).await;
    }
    Ok(json!({"cleared": cleared}))
}

async fn clear_bounce_h(State(state): State<AppState>, ctx: Ctx, ApiJson(b): ApiJson<AddressBody>) -> AppResult<Json<Value>> {
    Ok(Json(clear_bounce(&state, &ctx, &b.email).await?))
}

async fn add_address_h(State(state): State<AppState>, ctx: Ctx, ApiJson(b): ApiJson<AddressBody>) -> AppResult<Json<Value>> {
    Ok(Json(add_address(&state, &ctx, &b.email).await?))
}

async fn remove_address(state: &AppState, ctx: &Ctx, email: &str) -> AppResult<Value> {
    let removed = notify::remove_address(&state.db, ctx.account_id, email).await?;
    if removed {
        crate::features::team::audit(state, ctx, "flow.notify_address_remove", Some(&email.trim().to_lowercase()), json!({})).await;
    }
    Ok(json!({"removed": removed}))
}

async fn remove_address_h(State(state): State<AppState>, ctx: Ctx, ApiJson(b): ApiJson<AddressBody>) -> AppResult<Json<Value>> {
    Ok(Json(remove_address(&state, &ctx, &b.email).await?))
}

#[derive(Deserialize)]
struct MetricQuery {
    metric: String,
}

async fn metric_trust_h(Query(q): Query<MetricQuery>) -> Json<Value> {
    Json(json!({"metric": q.metric, "browser_can_send": browser_can_send(&q.metric)}))
}

#[derive(Deserialize)]
struct TokenQuery {
    t: Option<String>,
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

fn page(title: &str, body: &str) -> Response {
    let mut r = crate::public::page(title, body, "#115f42").into_response();
    r.headers_mut().insert("referrer-policy", axum::http::HeaderValue::from_static("no-referrer"));
    r.headers_mut().insert("cache-control", axum::http::HeaderValue::from_static("no-store"));
    r
}

const EXPIRED: &str = "<h1>This link doesn't work</h1><p>It may have expired or already been used. Ask whoever set up the notification to send a new one.</p>";

/// The link in the confirmation email shows a button; only pressing it confirms (mail scanners open links).
async fn confirm_page(State(state): State<AppState>, Query(q): Query<TokenQuery>) -> Response {
    let token = q.t.unwrap_or_default();
    match notify::link_account(&state.db, &token).await {
        Ok(Some((account, email))) => page(
            "Confirm notifications",
            &format!(
                "<h1>Get notifications from {a}?</h1><p>{a} wants to email <b>{e}</b> notifications from an automated flow in DumbMail.</p>\
                 <form method=\"post\" action=\"/flow-notify/confirm\"><input type=\"hidden\" name=\"t\" value=\"{t}\"><input type=\"submit\" value=\"Confirm notifications\"></form>\
                 <p style=\"font-size:13px\">If you don't want these, close this page. You'll get nothing.</p>",
                a = esc(&account),
                e = esc(&email),
                t = esc(&token)
            ),
        ),
        _ => page("Link expired", EXPIRED),
    }
}

async fn confirm_post(State(state): State<AppState>, Form(q): Form<TokenQuery>) -> Response {
    match notify::confirm(&state.db, q.t.as_deref().unwrap_or_default()).await {
        Ok(Some((account, email))) => page("Confirmed", &format!("<h1>You're set</h1><p><b>{}</b> will now get notifications from {}.</p>", esc(&email), esc(&account))),
        _ => page("Link expired", EXPIRED),
    }
}

fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/flows/{id}/results", get(results_h))
        .route("/api/flows/{id}/steps/{node_id}/waiting", get(waiting_h))
        .route("/api/flows/{id}/runs/remove", post(remove_h))
        .route("/api/flows/{id}/backfill", post(backfill_h))
        .route("/api/flows/{id}/backfills", get(backfills_h))
        .route("/api/flows/{id}/backfills/{backfill_id}/cancel", post(cancel_h))
        .route("/api/flows-plus/notify-addresses", get(addresses_h).post(add_address_h))
        .route("/api/flows-plus/notify-addresses/remove", post(remove_address_h))
        .route("/api/flows-plus/notify-addresses/send-confirmation", post(send_confirmation_h))
        .route("/api/flows-plus/notify-addresses/clear-bounce", post(clear_bounce_h))
        .route("/api/flows-plus/metric-trust", get(metric_trust_h))
        .route("/flow-notify/confirm", get(confirm_page).post(confirm_post))
}

// ---------- MCP ----------

fn uuid_arg(a: &Value, k: &str) -> AppResult<Uuid> {
    a[k].as_str().and_then(|s| s.parse().ok()).ok_or_else(|| AppError::bad(format!("`{k}` must be an id")))
}

fn tools() -> Vec<Value> {
    let flow_id = json!({"type": "string", "description": "The flow's id"});
    vec![
        tool("get_flow_results", "Per-step flow results",
            "For every step of a flow: people who reached it (runs that started in the range), people waiting there now, and for email and SMS steps sent, delivered, opened, clicked, bounced, unsubscribed, skipped (with the top reasons), failed, and orders and revenue credited to the step by the account's attribution settings; notification steps show sent, skipped and failed. Range: days (last N x 24 hours, default 30) or start/end dates (YYYY-MM-DD, account time zone).",
            obj(json!({"flow_id": flow_id, "days": {"type": "integer", "minimum": 1, "maximum": 731}, "start": {"type": "string", "format": "date"}, "end": {"type": "string", "format": "date"}}), &["flow_id"]), true, false),
        tool("list_flow_waiting", "People waiting at a flow step",
            "The people waiting at one step of a flow right now (for example in a delay), with their run id, when they entered the flow and when the wait ends if known. Paged: pass next_after as after.",
            obj(json!({"flow_id": flow_id, "node_id": {"type": "string"}, "limit": {"type": "integer", "minimum": 1, "maximum": results::MAX_PAGE}, "after": {"type": "string", "description": "run_id from next_after"}}), &["flow_id", "node_id"]), true, false),
        tool("remove_from_flow", "Take people out of a flow",
            "End the runs of up to 500 people in a flow (they get nothing more from it; they can enter again only as its re-entry rules allow). Without confirm: true it only says how many would be taken out.",
            obj(json!({"flow_id": flow_id, "run_ids": {"type": "array", "items": {"type": "string"}, "minItems": 1, "maxItems": results::MAX_REMOVE}, "confirm": {"type": "boolean"}}), &["flow_id", "run_ids"]), false, true),
        tool("backfill_flow", "Add people who already qualify to a flow",
            "Back-populate a live list- or segment-triggered flow with people who are in the trigger list or segment now (not metric flows: past events don't start flows; not date flows: people whose date is still ahead enter on their own). Without confirm: true it only counts them. With confirm it adds them in the background in paced batches; each person is checked again as they're added (still in the list or segment, the profile filter, the exit condition, re-entry rules) and consent is checked when each message goes out. One at a time per flow; capped (default 50,000; `limit` lowers it).",
            obj(json!({"flow_id": flow_id, "confirm": {"type": "boolean"}, "limit": {"type": "integer", "minimum": 1}}), &["flow_id"]), false, true),
        tool("list_flow_backfills", "List a flow's back-populates", "A flow's recent back-populates with their status (running, done, cancelled, failed) and how many people were added and skipped.",
            obj(json!({"flow_id": flow_id}), &["flow_id"]), true, false),
        tool("cancel_flow_backfill", "Stop a back-populate", "Stop adding people to a flow. People already added stay in the flow (take them out with remove_from_flow).",
            obj(json!({"flow_id": flow_id, "backfill_id": {"type": "string"}}), &["flow_id", "backfill_id"]), false, true),
        tool("list_notify_addresses", "List notification addresses",
            "Outside addresses (not team members) used by flow notification steps and their status: confirmed, unconfirmed, bounced or complained (with the reason). Pass emails to get the status of specific addresses (team, confirmed, unconfirmed, removed, bounced, complained or new). Only team members and confirmed addresses get notifications; bounced and complained addresses get none.",
            obj(json!({"emails": {"type": "array", "items": {"type": "string"}, "maxItems": 50}}), &[]), true, false),
        tool("add_notify_address", "Record a notification address",
            "Record an outside address for flow notifications as unconfirmed. Nothing is emailed; it gets nothing until it confirms (see send_notify_confirmation). Team members' addresses need no confirmation. At most 20 addresses can wait to confirm.",
            obj(json!({"email": {"type": "string"}}), &["email"]), false, false),
        tool("send_notify_confirmation", "Email an address a link to confirm notifications",
            "Email an outside address a link to confirm it wants this account's flow notifications. Without confirm: true it only checks. Refused while sending is paused, and for bounced or complaining addresses. Each address gets at most one a day, and an account sends at most 50 a day.",
            obj(json!({"email": {"type": "string"}, "confirm": {"type": "boolean"}}), &["email"]), false, true),
        tool("remove_notify_address", "Remove a notification address", "Remove an outside address: it stops getting flow notifications until it's confirmed again.",
            obj(json!({"email": {"type": "string"}}), &["email"]), false, true),
        tool("clear_notify_bounce", "Clear a notification address's bounce",
            "After fixing an address whose notification hard bounced, let it get notifications again. Complaints (marked as spam) can't be cleared.",
            obj(json!({"email": {"type": "string"}}), &["email"]), false, false),
    ]
}

async fn call(state: &AppState, ctx: &Ctx, name: &str, a: &Value) -> Option<AppResult<Value>> {
    let acct = ctx.account_id;
    let r = match name {
        "get_flow_results" => async {
            let date = |k: &str| a[k].as_str().map(|s| chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d").map_err(|_| AppError::bad(format!("{k} must be YYYY-MM-DD")))).transpose();
            results::results(state, acct, uuid_arg(a, "flow_id")?, results::Range { days: a["days"].as_i64(), start: date("start")?, end: date("end")? }).await
        }
        .await,
        "list_flow_waiting" => async {
            let after = match a["after"].as_str() {
                Some(_) => Some(uuid_arg(a, "after")?),
                None => None,
            };
            let node = a["node_id"].as_str().ok_or_else(|| AppError::bad("`node_id` is required"))?;
            results::waiting(state, acct, uuid_arg(a, "flow_id")?, node, results::WaitingQuery { limit: a["limit"].as_i64(), after }).await
        }
        .await,
        "remove_from_flow" => async {
            let b: results::RemoveBody = crate::mcp::from(json!({"run_ids": a["run_ids"], "confirm": a["confirm"].as_bool().unwrap_or(false)}))?;
            results::remove(state, ctx, uuid_arg(a, "flow_id")?, b).await
        }
        .await,
        "backfill_flow" => async { backfill::start(state, ctx, uuid_arg(a, "flow_id")?, a["confirm"].as_bool() == Some(true), a["limit"].as_i64()).await }.await,
        "list_flow_backfills" => async { Ok(json!({"data": backfill::list(state, acct, uuid_arg(a, "flow_id")?).await?})) }.await,
        "cancel_flow_backfill" => async { backfill::cancel(state, ctx, uuid_arg(a, "flow_id")?, uuid_arg(a, "backfill_id")?).await }.await,
        "list_notify_addresses" => async {
            let emails: Vec<String> = a["emails"].as_array().into_iter().flatten().filter_map(|v| v.as_str().map(str::to_string)).collect();
            addresses_json(state, acct, Some(&emails.join(","))).await
        }
        .await,
        "add_notify_address" => async { add_address(state, ctx, a["email"].as_str().unwrap_or("")).await }.await,
        "remove_notify_address" => async { remove_address(state, ctx, a["email"].as_str().unwrap_or("")).await }.await,
        "send_notify_confirmation" => async { send_confirmation(state, ctx, a["email"].as_str().unwrap_or(""), a["confirm"].as_bool() == Some(true)).await }.await,
        "clear_notify_bounce" => async { clear_bounce(state, ctx, a["email"].as_str().unwrap_or("")).await }.await,
        _ => return None,
    };
    Some(r)
}

#[async_trait]
impl Feature for Module {
    fn name(&self) -> &'static str {
        "flows_plus"
    }

    fn routes(&self) -> Router<AppState> {
        routes()
    }

    fn access(&self) -> Access {
        Access { routes: ROUTES, tools: TOOLS }
    }

    fn mcp_tools(&self) -> Vec<Value> {
        tools()
    }

    async fn mcp_call(&self, state: &AppState, ctx: &Ctx, name: &str, args: &Value) -> Option<AppResult<Value>> {
        call(state, ctx, name, args).await
    }

    fn mcp_docs(&self) -> Vec<McpDoc> {
        vec![McpDoc {
            slug: "flows_plus",
            name: "Flow tools",
            description: "Delays until a time, weekday or date; trigger splits on the event; notification steps; back-populating flows; per-step results and who is waiting where",
            text: DOC,
        }]
    }

    async fn run_job(&self, state: &AppState, kind: &str, payload: &Value) -> Option<anyhow::Result<()>> {
        match kind {
            backfill::JOB => Some(backfill::run(state, payload).await),
            notify::CONFIRM_JOB => Some(notify::send_confirmation(state, payload).await),
            _ => None,
        }
    }

    async fn tick(&self, state: &AppState, tick: u64) -> anyhow::Result<()> {
        if tick.is_multiple_of(15) {
            notify::sweep(state).await?;
        }
        Ok(())
    }
}
