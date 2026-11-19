//! RCS: texts upgraded to rich messages, through Telnyx, for phones that can get them.
//!
//! RCS is a branch of SMS, not a channel of its own: the same consent, quiet hours, STOP and HELP,
//! the same number's opt-out lock and claim, the same inbox. With an RCS agent saved, confirmed by
//! a Check and switched on (Settings → SMS), a campaign or flow text to a phone Telnyx says can get
//! RCS goes from the agent, with the text's optional rich version (a card, suggested replies and
//! links); everyone else gets the same text as SMS, exactly as before. A message Telnyx reports
//! undelivered falls back to SMS once. Billed once: at `PRICE_RCS` as RCS, or as SMS when it fell
//! back. See docs/RCS.md and [`send`] for the steps.

pub mod capability;
pub mod content;
pub mod report;
pub mod send;
pub mod settings;
pub mod telnyx;
mod webhook;

use async_trait::async_trait;
use axum::extract::{DefaultBodyLimit, Query, State};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};
use uuid::Uuid;

use super::{Feature, McpDoc};
use crate::api::ApiJson;
use crate::auth::Ctx;
use crate::error::{AppError, AppResult};
use crate::features::team::access::{public, rule, Access, Perm, RouteRule, ToolRule};
use crate::mcp::{from, obj, tool};
use crate::state::AppState;

pub struct Module;

/// The event a read receipt records (server-only: see `audience::SERVER_ONLY_METRICS`).
pub const READ_METRIC: &str = "Read SMS";

/// Operator configuration.
#[derive(clap::Args, Debug, Clone, Default)]
pub struct RcsConfig {
    /// Per RCS message, in dollars, charged from the balance like texts. Unset: the price of one
    /// SMS segment (PRICE_SMS_SEGMENT). A text that falls back to SMS is billed as SMS instead.
    #[arg(long, env = "PRICE_RCS")]
    pub price_rcs: Option<f64>,
    /// A loopback stand-in for Telnyx's RCS API, for tests only. It can't be set from the
    /// environment or the command line, and anything other than `http://127.0.0.1:…` or
    /// `http://localhost:…` is ignored.
    #[arg(skip)]
    pub api_base: Option<String>,
}

/// Settings read like the account's other settings; changing them and the Check (which records
/// what Telnyx said) are settings write; the report is messaging data.
static ROUTES: &[RouteRule] = &[
    rule("GET", "/api/rcs/settings", Perm::AccountRead),
    rule("PATCH", "/api/rcs/settings", Perm::SettingsWrite),
    rule("POST", "/api/rcs/settings/check", Perm::SettingsWrite),
    rule("GET", "/api/rcs/report", Perm::MessagingRead),
    public("POST", "/webhooks/rcs/{token}"),
];

static TOOLS: &[ToolRule] = &[
    ToolRule { name: "get_rcs_settings", perm: Perm::AccountRead },
    ToolRule { name: "update_rcs_settings", perm: Perm::SettingsWrite },
    ToolRule { name: "check_rcs_agent", perm: Perm::SettingsWrite },
    ToolRule { name: "get_rcs_report", perm: Perm::MessagingRead },
];

const DOC: &str = include_str!("../../../docs/RCS.md");

// ---------- Hooks used by shared code ----------

/// Problems with a text's rich version (campaign readiness, previews, flow go-live).
pub fn content_problems(content: &Value) -> Vec<String> {
    content::problems(content)
}

/// The rich version as a preview shows it (the title filled in for the sample person), or null.
/// Refuses an invalid one, so a campaign or flow with one can't launch.
pub fn preview(content: &Value, ctx: &Value) -> AppResult<Value> {
    let Some(rich) = content::parse(content)? else { return Ok(Value::Null) };
    let p = content::prepare(Some(&rich), ctx, None)?;
    Ok(json!({
        "card": rich.card.as_ref().map(|_| json!({"image_url": p.image_url, "title": p.title})),
        "suggestions": rich.suggestions.iter().map(|s| json!({"type": s.kind, "text": s.text.trim(), "url": s.url})).collect::<Vec<_>>(),
    }))
}

/// The most a campaign of `recipients` texts could cost as RCS (launch checks use the larger of
/// this and the SMS estimate, since each text goes one way or the other). 0 when RCS is off.
pub async fn campaign_cost(state: &AppState, account_id: Uuid, recipients: i64) -> AppResult<i64> {
    match settings::active(state, account_id).await? {
        Some(a) => Ok(recipients.max(0) * settings::price(state, a.creds.own)),
        None => Ok(0),
    }
}

// ---------- REST ----------

async fn settings_h(State(state): State<AppState>, ctx: Ctx) -> AppResult<Json<Value>> {
    Ok(Json(settings::settings_json(&state, ctx.account_id).await?))
}

pub async fn update_settings(state: &AppState, ctx: &Ctx, b: settings::SettingsBody) -> AppResult<Value> {
    let changed = settings::update(state, ctx.account_id, b).await?;
    if !changed.is_empty() {
        crate::features::team::audit(state, ctx, "rcs.settings_update", None, json!({"changed": changed})).await;
    }
    settings::settings_json(state, ctx.account_id).await
}

async fn update_settings_h(State(state): State<AppState>, ctx: Ctx, ApiJson(b): ApiJson<settings::SettingsBody>) -> AppResult<Json<Value>> {
    Ok(Json(update_settings(&state, &ctx, b).await?))
}

pub async fn check(state: &AppState, ctx: &Ctx) -> AppResult<Value> {
    let out = settings::check(state, ctx.account_id).await?;
    crate::features::team::audit(state, ctx, "rcs.agent_check", None, json!({"confirmed": out["confirmed"]})).await;
    Ok(out)
}

async fn check_h(State(state): State<AppState>, ctx: Ctx) -> AppResult<Json<Value>> {
    Ok(Json(check(&state, &ctx).await?))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReportQuery {
    campaign_id: Option<Uuid>,
    flow_id: Option<Uuid>,
}

async fn report_h(State(state): State<AppState>, ctx: Ctx, Query(q): Query<ReportQuery>) -> AppResult<Json<Value>> {
    Ok(Json(report::report(&state, ctx.account_id, q.campaign_id, q.flow_id).await?))
}

// ---------- MCP ----------

fn tools() -> Vec<Value> {
    let id = |d: &str| json!({"type": "string", "description": d});
    vec![
        tool("get_rcs_settings", "Get RCS settings", "The account's RCS agent for texts: the agent id, whether Telnyx confirmed it (the last Check), whether RCS is on and sending, the webhook URL to set on the agent in Telnyx, and the prices. RCS goes through Telnyx: DumbMail's numbers or the account's own Telnyx account.",
            obj(json!({}), &[]), true, false),
        tool("update_rcs_settings", "Change RCS settings", "Save the RCS agent id from Telnyx (a new id turns RCS off until check_rcs_agent confirms it; null forgets it) and switch RCS on or off. On: texts to phones that can get RCS go from the agent as RCS (with the text's rich version), billed at the RCS price; everyone else gets SMS as before. Needs a confirmed agent and Telnyx's webhook signing key.",
            obj(json!({"agent_id": {"type": ["string", "null"]}, "enabled": {"type": "boolean"}}), &[]), false, false),
        tool("check_rcs_agent", "Check the RCS agent with Telnyx", "Ask Telnyx about the saved agent: it must exist for this texting connection, be enabled, and have its webhook URL set to this account's RCS webhook URL (get_rcs_settings). The result shows in confirmed and check_problem; a definite problem turns RCS off.",
            obj(json!({}), &[]), false, false),
        tool("get_rcs_report", "RCS and SMS results", "For an SMS campaign or a flow: texts that went as RCS and as SMS, each with sent, delivered and clicked; read for RCS (read receipts). Fallback texts count as SMS.",
            obj(json!({"campaign_id": id("Campaign id."), "flow_id": id("Flow id.")}), &[]), true, false),
    ]
}

async fn call(state: &AppState, ctx: &Ctx, name: &str, args: &Value) -> AppResult<Value> {
    match name {
        "get_rcs_settings" => settings::settings_json(state, ctx.account_id).await,
        "update_rcs_settings" => update_settings(state, ctx, from(args.clone())?).await,
        "check_rcs_agent" => check(state, ctx).await,
        "get_rcs_report" => {
            let uuid = |k: &str| -> AppResult<Option<Uuid>> {
                args.get(k).and_then(Value::as_str).map(|s| s.parse().map_err(|_| AppError::bad(format!("{k} must be a UUID")))).transpose()
            };
            report::report(state, ctx.account_id, uuid("campaign_id")?, uuid("flow_id")?).await
        }
        _ => Err(AppError::not_found("tool")),
    }
}

#[async_trait]
impl Feature for Module {
    fn name(&self) -> &'static str {
        "rcs"
    }

    fn routes(&self) -> Router<AppState> {
        Router::new()
            .route("/api/rcs/settings", get(settings_h).patch(update_settings_h))
            .route("/api/rcs/settings/check", post(check_h))
            .route("/api/rcs/report", get(report_h))
            .route("/webhooks/rcs/{token}", post(webhook::handle).layer(DefaultBodyLimit::max(256 * 1024)))
    }

    fn access(&self) -> Access {
        Access { routes: ROUTES, tools: TOOLS }
    }

    fn mcp_tools(&self) -> Vec<Value> {
        tools()
    }

    async fn mcp_call(&self, state: &AppState, ctx: &Ctx, name: &str, args: &Value) -> Option<AppResult<Value>> {
        if !TOOLS.iter().any(|t| t.name == name) {
            return None;
        }
        Some(call(state, ctx, name, args).await)
    }

    fn mcp_docs(&self) -> Vec<McpDoc> {
        vec![McpDoc { slug: "rcs", name: "RCS texts", description: "RCS for texts through Telnyx: setup, the rich version, fallback to SMS, billing and reports.", text: DOC }]
    }

    async fn run_job(&self, state: &AppState, kind: &str, payload: &Value) -> Option<anyhow::Result<()>> {
        let id = || payload.get("message_id").or_else(|| payload.get("campaign_id")).and_then(Value::as_str).and_then(|s| s.parse::<Uuid>().ok());
        match kind {
            "rcs_fallback" => Some(match id() {
                Some(mid) => send::fallback(state, mid).await,
                None => Ok(()),
            }),
            "rcs_prefetch" => Some(match id() {
                Some(cid) => capability::prefetch(state, cid).await,
                None => Ok(()),
            }),
            _ => None,
        }
    }

    async fn tick(&self, state: &AppState, tick: u64) -> anyhow::Result<()> {
        capability::start_prefetches(state).await?;
        if tick.is_multiple_of(180) {
            capability::forget_old(state).await?;
        }
        Ok(())
    }
}
