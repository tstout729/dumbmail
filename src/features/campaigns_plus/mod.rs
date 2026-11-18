//! Campaign tools people moving from Klaviyo expect: link tagging (UTM), resending to people who
//! didn't open or click, tags, archiving and search for campaigns, flows and templates, and a
//! campaign calendar.
//!
//! Hooks in the shared code, each a line or two:
//! - `render::render_email_hooked` / `render_sms_hooked` take a [`crate::render::LinkHook`]; the send
//!   path builds one with [`utm::Tagger::from_content`].
//! - `campaigns::prepare_send` calls [`utm::freeze_campaign`]; `campaigns::send_test` calls [`utm::for_test`];
//!   `flows::step` loads the flow with [`utm::FLOW_WITH_DEFAULTS_SQL`] and calls [`utm::for_flow_step`].
//! - `campaigns::resolve` knows the `resend` audience ([`resend::resolve`]); `campaigns::readiness`
//!   checks it ([`resend::audience_problems`]).
//! - The campaign, flow and template lists use [`tags::campaigns`], [`tags::flows`] and
//!   [`tags::templates`]; `messaging::launch` and `messaging::set_flow_status` refuse archived items.

pub mod calendar;
pub mod resend;
pub mod tags;
pub mod utm;

use async_trait::async_trait;
use axum::extract::{Path, Query, State};
use axum::routing::{get, patch, post};
use axum::{Json, Router};
use chrono::NaiveDate;
use serde::Deserialize;
use serde_json::{json, Value};
use uuid::Uuid;

use super::{Feature, McpDoc};
use crate::api::ApiJson;
use crate::auth::Ctx;
use crate::error::{AppError, AppResult};
use crate::features::team::access::{rule, Access, Perm, RouteRule, ToolRule};
use crate::features::team::audit;
use crate::mcp::{from, obj, tool};
use crate::state::AppState;
use tags::Kind;

pub struct Module;

/// Reading is messaging read; tags, archive and moving a campaign are campaign management (flows:
/// flow editing; templates: content writing); a resend draft is a campaign draft and sending it is
/// an ordinary send; the account's link tag defaults are a setting.
static ROUTES: &[RouteRule] = &[
    rule("GET", "/api/link-tags", Perm::AccountRead),
    rule("PUT", "/api/link-tags", Perm::SettingsWrite),
    rule("GET", "/api/campaigns/{id}/link-tags", Perm::MessagingRead),
    rule("PUT", "/api/campaigns/{id}/link-tags", Perm::CampaignDraft),
    rule("GET", "/api/flows/{id}/link-tags", Perm::MessagingRead),
    rule("PUT", "/api/flows/{id}/link-tags", Perm::FlowEdit),
    rule("POST", "/api/campaigns/{id}/resend", Perm::CampaignDraft),
    rule("GET", "/api/campaigns/{id}/resends", Perm::MessagingRead),
    rule("GET", "/api/tags", Perm::MessagingRead),
    rule("PATCH", "/api/tags/{id}", Perm::CampaignManage),
    rule("DELETE", "/api/tags/{id}", Perm::CampaignManage),
    rule("POST", "/api/campaigns/tags", Perm::CampaignManage),
    rule("POST", "/api/flows/tags", Perm::FlowEdit),
    rule("POST", "/api/templates/tags", Perm::ContentWrite),
    rule("POST", "/api/campaigns/archive", Perm::CampaignManage),
    rule("POST", "/api/flows/archive", Perm::FlowEdit),
    rule("POST", "/api/templates/archive", Perm::ContentWrite),
    rule("GET", "/api/campaigns/counts", Perm::MessagingRead),
    rule("GET", "/api/campaigns/calendar", Perm::MessagingRead),
    rule("POST", "/api/campaigns/{id}/reschedule", Perm::CampaignManage),
];

static TOOLS: &[ToolRule] = &[
    ToolRule { name: "get_link_tagging", perm: Perm::AccountRead },
    ToolRule { name: "update_link_tagging", perm: Perm::SettingsWrite },
    ToolRule { name: "get_campaign_link_tags", perm: Perm::MessagingRead },
    ToolRule { name: "set_campaign_link_tags", perm: Perm::CampaignDraft },
    ToolRule { name: "get_flow_link_tags", perm: Perm::MessagingRead },
    ToolRule { name: "set_flow_link_tags", perm: Perm::FlowEdit },
    ToolRule { name: "create_resend", perm: Perm::CampaignDraft },
    ToolRule { name: "get_campaign_resends", perm: Perm::MessagingRead },
    ToolRule { name: "list_tags", perm: Perm::MessagingRead },
    ToolRule { name: "rename_tag", perm: Perm::CampaignManage },
    ToolRule { name: "delete_tag", perm: Perm::CampaignManage },
    ToolRule { name: "tag_campaigns", perm: Perm::CampaignManage },
    ToolRule { name: "tag_flows", perm: Perm::FlowEdit },
    ToolRule { name: "tag_templates", perm: Perm::ContentWrite },
    ToolRule { name: "archive_campaigns", perm: Perm::CampaignManage },
    ToolRule { name: "archive_flows", perm: Perm::FlowEdit },
    ToolRule { name: "archive_templates", perm: Perm::ContentWrite },
    ToolRule { name: "search_campaigns", perm: Perm::MessagingRead },
    ToolRule { name: "campaign_calendar", perm: Perm::MessagingRead },
    ToolRule { name: "reschedule_campaign", perm: Perm::CampaignManage },
];

const DOC: &str = include_str!("../../../docs/CAMPAIGNS_PLUS.md");

// ---------- Handlers ----------

async fn get_defaults_h(State(state): State<AppState>, ctx: Ctx) -> AppResult<Json<Value>> {
    Ok(Json(utm::get_defaults(&state, ctx.account_id).await?))
}

async fn update_defaults(state: &AppState, ctx: &Ctx, b: utm::DefaultsBody) -> AppResult<Value> {
    let (v, changed) = utm::put_defaults(state, ctx.account_id, b).await?;
    audit::audit(state, ctx, "link_tags.update", None, json!({"changed": changed, "enabled": v["enabled"]})).await;
    Ok(v)
}

async fn put_defaults_h(State(state): State<AppState>, ctx: Ctx, ApiJson(b): ApiJson<utm::DefaultsBody>) -> AppResult<Json<Value>> {
    Ok(Json(update_defaults(&state, &ctx, b).await?))
}

async fn campaign_tags_h(State(state): State<AppState>, ctx: Ctx, Path(id): Path<Uuid>) -> AppResult<Json<Value>> {
    Ok(Json(utm::campaign_tags(&state, ctx.account_id, id).await?))
}

async fn set_campaign_tags_h(State(state): State<AppState>, ctx: Ctx, Path(id): Path<Uuid>, ApiJson(b): ApiJson<utm::OverrideBody>) -> AppResult<Json<Value>> {
    Ok(Json(utm::set_campaign(&state, ctx.account_id, id, b, crate::api::messaging::editable_for(&ctx)).await?))
}

async fn flow_tags_h(State(state): State<AppState>, ctx: Ctx, Path(id): Path<Uuid>) -> AppResult<Json<Value>> {
    Ok(Json(utm::flow_tags(&state, ctx.account_id, id).await?))
}

async fn set_flow_tags_h(State(state): State<AppState>, ctx: Ctx, Path(id): Path<Uuid>, ApiJson(b): ApiJson<utm::OverrideBody>) -> AppResult<Json<Value>> {
    Ok(Json(utm::set_flow(&state, ctx.account_id, id, b).await?))
}

async fn create_resend(state: &AppState, ctx: &Ctx, id: Uuid, b: resend::CreateBody) -> AppResult<Value> {
    let c = resend::create(state, ctx.account_id, id, b).await?;
    audit::campaign(state, ctx, "campaign.resend_create", &c).await;
    crate::api::messaging::campaign_json(state, &c).await
}

async fn create_resend_h(State(state): State<AppState>, ctx: Ctx, Path(id): Path<Uuid>, body: axum::body::Bytes) -> AppResult<Json<Value>> {
    let b: resend::CreateBody = if body.iter().all(u8::is_ascii_whitespace) {
        resend::CreateBody::default()
    } else {
        serde_json::from_slice(&body).map_err(|e| AppError::bad(format!("invalid body: {e}")))?
    };
    Ok(Json(create_resend(&state, &ctx, id, b).await?))
}

async fn resends_h(State(state): State<AppState>, ctx: Ctx, Path(id): Path<Uuid>) -> AppResult<Json<Value>> {
    Ok(Json(resend::summary(&state, ctx.account_id, id).await?))
}

async fn tags_h(State(state): State<AppState>, ctx: Ctx) -> AppResult<Json<Value>> {
    Ok(Json(tags::list_tags(&state, ctx.account_id).await?))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RenameBody {
    name: String,
}

async fn rename_tag_h(State(state): State<AppState>, ctx: Ctx, Path(id): Path<Uuid>, ApiJson(b): ApiJson<RenameBody>) -> AppResult<Json<Value>> {
    Ok(Json(tags::rename(&state, ctx.account_id, id, &b.name).await?))
}

async fn delete_tag_h(State(state): State<AppState>, ctx: Ctx, Path(id): Path<Uuid>) -> AppResult<Json<Value>> {
    tags::delete(&state, ctx.account_id, id).await?;
    audit::audit(&state, &ctx, "tag.delete", Some(&id.to_string()), json!({})).await;
    Ok(Json(json!({"ok": true})))
}

async fn apply_tags(state: &AppState, ctx: &Ctx, kind: Kind, b: tags::ApplyBody) -> AppResult<Value> {
    tags::apply(state, ctx.account_id, kind, b).await
}

async fn archive(state: &AppState, ctx: &Ctx, kind: Kind, b: tags::ArchiveBody) -> AppResult<Value> {
    let archived = b.archived.unwrap_or(true);
    let v = tags::archive(state, ctx.account_id, kind, b).await?;
    let noun = match kind {
        Kind::Campaign => "campaign",
        Kind::Flow => "flow",
        Kind::Template => "template",
    };
    for id in v["changed"].as_array().into_iter().flatten().filter_map(|i| i.as_str()) {
        audit::audit(state, ctx, &format!("{noun}.{}", if archived { "archive" } else { "unarchive" }), Some(id), json!({})).await;
    }
    Ok(v)
}

macro_rules! kind_handlers {
    ($tag:ident, $arch:ident, $kind:expr) => {
        async fn $tag(State(state): State<AppState>, ctx: Ctx, ApiJson(b): ApiJson<tags::ApplyBody>) -> AppResult<Json<Value>> {
            Ok(Json(apply_tags(&state, &ctx, $kind, b).await?))
        }
        async fn $arch(State(state): State<AppState>, ctx: Ctx, ApiJson(b): ApiJson<tags::ArchiveBody>) -> AppResult<Json<Value>> {
            Ok(Json(archive(&state, &ctx, $kind, b).await?))
        }
    };
}
kind_handlers!(tag_campaigns_h, archive_campaigns_h, Kind::Campaign);
kind_handlers!(tag_flows_h, archive_flows_h, Kind::Flow);
kind_handlers!(tag_templates_h, archive_templates_h, Kind::Template);

async fn counts_h(State(state): State<AppState>, ctx: Ctx) -> AppResult<Json<Value>> {
    Ok(Json(tags::campaign_counts(&state, ctx.account_id).await?))
}

#[derive(Deserialize)]
struct CalendarQuery {
    from: String,
    to: String,
    #[serde(default)]
    include_archived: Option<String>,
}

fn date(s: &str, name: &str) -> AppResult<NaiveDate> {
    NaiveDate::parse_from_str(s, "%Y-%m-%d").map_err(|_| AppError::bad(format!("{name} must be a date like 2026-11-27")))
}

async fn calendar_h(State(state): State<AppState>, ctx: Ctx, Query(q): Query<CalendarQuery>) -> AppResult<Json<Value>> {
    let all = matches!(q.include_archived.as_deref(), Some("true" | "1"));
    Ok(Json(calendar::calendar(&state, ctx.account_id, date(&q.from, "from")?, date(&q.to, "to")?, all).await?))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RescheduleBody {
    /// The new day, YYYY-MM-DD in the account's time zone; the time of day stays.
    date: String,
}

async fn reschedule(state: &AppState, ctx: &Ctx, id: Uuid, day: NaiveDate) -> AppResult<Value> {
    let (c, old) = calendar::reschedule(state, ctx.account_id, id, day).await?;
    audit::audit(state, ctx, "campaign.reschedule", Some(&c.id.to_string()), json!({"name": c.name, "from": old, "to": c.send_at})).await;
    crate::api::messaging::campaign_json(state, &c).await
}

async fn reschedule_h(State(state): State<AppState>, ctx: Ctx, Path(id): Path<Uuid>, ApiJson(b): ApiJson<RescheduleBody>) -> AppResult<Json<Value>> {
    Ok(Json(reschedule(&state, &ctx, id, date(&b.date, "date")?).await?))
}

// ---------- MCP ----------

fn uuid_arg(a: &Value, k: &str) -> AppResult<Uuid> {
    a[k].as_str().and_then(|s| s.parse().ok()).ok_or_else(|| AppError::bad(format!("`{k}` must be an id")))
}

fn params_schema() -> Value {
    let v = |d: &str| json!({"type": ["string", "null"], "maxLength": 200, "description": d});
    json!({
        "type": "object",
        "additionalProperties": false,
        "description": "Values may use {campaign_name}, {campaign_id}, {flow_name}, {flow_id}, {message_name}, {channel}, {send_date} (YYYY-MM-DD, account time zone) and {version} (A/B version). No other braces.",
        "properties": {
            "source": v("utm_source; empty: dumbmail"),
            "medium": v("utm_medium; empty: {channel} (email or sms)"),
            "campaign": v("utm_campaign; empty: the campaign or flow name, slugged, plus the first 8 characters of its id"),
            "content": v("utm_content; empty: left out"),
            "term": v("utm_term; empty: left out"),
            "custom": {"type": "array", "maxItems": 10, "items": {"type": "object", "additionalProperties": false, "required": ["name", "value"], "properties": {"name": {"type": "string", "pattern": "^[a-z0-9_]{1,40}$"}, "value": {"type": "string", "maxLength": 200}}}}
        }
    })
}

fn override_schema(extra: Value) -> Value {
    let mut props = json!({"mode": {"enum": ["default", "off", "custom"], "description": "default: the account's settings; off: no tags; custom: these params"}, "params": params_schema()});
    for (k, v) in extra.as_object().into_iter().flatten() {
        props[k] = v.clone();
    }
    props
}

fn ids_schema(noun: &str) -> Value {
    json!({"type": "array", "minItems": 1, "maxItems": 100, "items": {"type": "string"}, "description": format!("{noun} ids")})
}

#[async_trait]
impl Feature for Module {
    fn name(&self) -> &'static str {
        "campaigns_plus"
    }

    fn access(&self) -> Access {
        Access { routes: ROUTES, tools: TOOLS }
    }

    fn routes(&self) -> Router<AppState> {
        Router::new()
            .route("/api/link-tags", get(get_defaults_h).put(put_defaults_h))
            .route("/api/campaigns/{id}/link-tags", get(campaign_tags_h).put(set_campaign_tags_h))
            .route("/api/flows/{id}/link-tags", get(flow_tags_h).put(set_flow_tags_h))
            .route("/api/campaigns/{id}/resend", post(create_resend_h))
            .route("/api/campaigns/{id}/resends", get(resends_h))
            .route("/api/tags", get(tags_h))
            .route("/api/tags/{id}", patch(rename_tag_h).delete(delete_tag_h))
            .route("/api/campaigns/tags", post(tag_campaigns_h))
            .route("/api/flows/tags", post(tag_flows_h))
            .route("/api/templates/tags", post(tag_templates_h))
            .route("/api/campaigns/archive", post(archive_campaigns_h))
            .route("/api/flows/archive", post(archive_flows_h))
            .route("/api/templates/archive", post(archive_templates_h))
            .route("/api/campaigns/counts", get(counts_h))
            .route("/api/campaigns/calendar", get(calendar_h))
            .route("/api/campaigns/{id}/reschedule", post(reschedule_h))
    }

    fn mcp_tools(&self) -> Vec<Value> {
        let mut tools = vec![
            tool("get_link_tagging", "Get link tagging (UTM) settings", "The account's UTM settings: whether campaign and flow links get UTM parameters, the values (with their fallbacks), the variables values may use, and domains that are never tagged.", obj(json!({}), &[]), true, false),
            tool("update_link_tagging", "Set link tagging (UTM) defaults", "Turn UTM link tagging on or off for the account and set the default values and the domains never tagged (at most 50). Fields left out keep their value. Applies to campaigns that start sending afterwards and to flow messages from now on. Links already in someone's parameter are kept; DumbMail's own links, mailto: and tel: are never tagged.",
                obj(json!({"enabled": {"type": "boolean"}, "params": params_schema(), "skip_domains": {"type": "array", "maxItems": 50, "items": {"type": "string"}}}), &[]), false, false),
            tool("get_campaign_link_tags", "Get a campaign's link tags", "What the campaign's links will be tagged with: its choice (default, off or custom), whether the account has tagging on, and the parameters with variables filled in as if it went out now (preview).", obj(json!({"campaign_id": {"type": "string"}}), &["campaign_id"]), true, false),
            tool("set_campaign_link_tags", "Set a campaign's link tags", "Use the account's UTM defaults for this campaign, turn tagging off for it, or give it its own values (custom). Only while it can be edited.", obj(override_schema(json!({"campaign_id": {"type": "string"}})), &["campaign_id", "mode"]), false, false),
            tool("get_flow_link_tags", "Get a flow's link tags", "What the flow's message links will be tagged with: its choice, the account's setting and a preview.", obj(json!({"flow_id": {"type": "string"}}), &["flow_id"]), true, false),
            tool("set_flow_link_tags", "Set a flow's link tags", "Use the account's UTM defaults for this flow, turn tagging off for it, or give it its own values. Takes effect for its next messages.", obj(override_schema(json!({"flow_id": {"type": "string"}})), &["flow_id", "mode"]), false, false),
            tool("create_resend", "Create a resend to non-openers", "Make a draft email campaign that resends a sent email campaign to the people it went to who haven't opened it (target no_open, default) or clicked it (no_click), optionally with a new subject and preview text. Nothing is sent: review it, then send it with send_campaign (which needs confirm=true). The audience is fixed when the resend starts sending: people whose original went out, who haven't opened/clicked the original or an earlier resend by then, and who are subscribed. Lists and segments don't apply. The original must have finished sending at least 4 hours and at most 30 days ago; at most 2 resends per campaign; a resend can't be resent. Opens are undercounted by mail privacy features, so some readers get it again.",
                obj(json!({"campaign_id": {"type": "string"}, "target": {"enum": ["no_open", "no_click"]}, "subject": {"type": "string", "maxLength": 300}, "preheader": {"type": "string", "maxLength": 300}, "name": {"type": "string", "maxLength": 200}}), &["campaign_id"]), false, false),
            tool("get_campaign_resends", "Get a campaign's resends", "For a campaign: its resends with their stats, combined totals across it and its resends (people reached, opened and clicked, each person counted once) and whether it can be resent now (and when). For a resend: which campaign it resends.", obj(json!({"campaign_id": {"type": "string"}}), &["campaign_id"]), true, false),
            tool("list_tags", "List tags", "The account's tags with how many campaigns, flows and templates use each, and the limits (20 per item, 200 per account, 40 characters).", obj(json!({}), &[]), true, false),
            tool("rename_tag", "Rename a tag", "Rename a tag everywhere it's used.", obj(json!({"tag_id": {"type": "string"}, "name": {"type": "string", "maxLength": 40}}), &["tag_id", "name"]), false, false),
            tool("delete_tag", "Delete a tag", "Delete a tag and take it off every campaign, flow and template. Without confirm=true it only says what would happen.", obj(json!({"tag_id": {"type": "string"}, "confirm": {"type": "boolean"}}), &["tag_id"]), false, true),
            tool("search_campaigns", "Search campaigns", "Campaigns matching filters, newest send date first, with stats, tags, archived_at and resend_of, and the total matching. Archived campaigns are left out unless archived is true or all.",
                obj(json!({"q": {"type": "string", "maxLength": 200, "description": "Text in the name or subject"}, "tag": {"type": "string", "description": "A tag id (list_tags)"}, "status": {"type": "string", "description": "One status or several, comma-separated (draft, scheduled, sending, testing, sent, cancelled, failed)"}, "channel": {"enum": ["email", "sms"]}, "archived": {"enum": ["false", "true", "all"]}, "from": {"type": "string", "format": "date"}, "to": {"type": "string", "format": "date"}, "limit": {"type": "integer", "minimum": 1, "maximum": 500}, "offset": {"type": "integer", "minimum": 0}}), &[]), true, false),
            tool("campaign_calendar", "Campaign calendar", "Campaigns placed on the days from..to (at most 62 days; YYYY-MM-DD in the account's time zone): scheduled, sending and sent ones and drafts with a planned date, with channel, status, local date and time, send timing and a note for local-time and best-time sends (shown on the day they start), and whether each can be moved.",
                obj(json!({"from": {"type": "string", "format": "date"}, "to": {"type": "string", "format": "date"}, "include_archived": {"type": "boolean"}}), &["from", "to"]), true, false),
            tool("reschedule_campaign", "Move a scheduled campaign to another day", "Move a scheduled campaign to another day (YYYY-MM-DD, account time zone), keeping its time of day; a local-time send moves its day by the same amount. Goes through the normal scheduling checks. Not allowed within 15 minutes of sending or to a time that would send it right away. This changes when real people get it: without confirm=true it returns when it would go out.",
                obj(json!({"campaign_id": {"type": "string"}, "date": {"type": "string", "format": "date"}, "confirm": {"type": "boolean"}}), &["campaign_id", "date"]), false, true),
        ];
        for (noun, plural) in [("campaign", "campaigns"), ("flow", "flows"), ("template", "templates")] {
            tools.push(tool(&format!("tag_{plural}"), &format!("Tag {plural}"), &format!("Add and remove tags (by name; new names become tags) on up to 100 {plural}. Returns each one's tags."),
                obj(json!({"ids": ids_schema(noun), "add": {"type": "array", "maxItems": 20, "items": {"type": "string", "maxLength": 40}}, "remove": {"type": "array", "maxItems": 200, "items": {"type": "string", "maxLength": 40}}}), &["ids"]), false, false));
            let rule = match noun {
                "campaign" => " Scheduled, sending and A/B-testing campaigns are skipped (unschedule or cancel first). Archived campaigns can't be sent or scheduled.",
                "flow" => " Live flows are skipped (turn them off first). Archived flows can't be turned on.",
                _ => "",
            };
            tools.push(tool(&format!("archive_{plural}"), &format!("Archive {plural}"), &format!("Archive (or with archived=false, unarchive) up to 100 {plural}. Archived ones are hidden from lists unless asked for and stay in reports.{rule}"),
                obj(json!({"ids": ids_schema(noun), "archived": {"type": "boolean", "description": "Default true"}}), &["ids"]), false, false));
        }
        tools
    }

    async fn mcp_call(&self, state: &AppState, ctx: &Ctx, name: &str, a: &Value) -> Option<AppResult<Value>> {
        let acct = ctx.account_id;
        let without = |keys: &[&str]| {
            let mut v = a.clone();
            if let Some(o) = v.as_object_mut() {
                for k in keys {
                    o.remove(*k);
                }
            }
            v
        };
        let kind_of = |name: &str| match name.split_once('_').map(|(_, p)| p) {
            Some("campaigns") => Kind::Campaign,
            Some("flows") => Kind::Flow,
            _ => Kind::Template,
        };
        let r = match name {
            "get_link_tagging" => utm::get_defaults(state, acct).await,
            "update_link_tagging" => async { update_defaults(state, ctx, from(a.clone())?).await }.await,
            "get_campaign_link_tags" => async { utm::campaign_tags(state, acct, uuid_arg(a, "campaign_id")?).await }.await,
            "set_campaign_link_tags" => async { utm::set_campaign(state, acct, uuid_arg(a, "campaign_id")?, from(without(&["campaign_id"]))?, crate::api::messaging::editable_for(ctx)).await }.await,
            "get_flow_link_tags" => async { utm::flow_tags(state, acct, uuid_arg(a, "flow_id")?).await }.await,
            "set_flow_link_tags" => async { utm::set_flow(state, acct, uuid_arg(a, "flow_id")?, from(without(&["flow_id"]))?).await }.await,
            "create_resend" => async { create_resend(state, ctx, uuid_arg(a, "campaign_id")?, from(without(&["campaign_id"]))?).await }.await,
            "get_campaign_resends" => async { resend::summary(state, acct, uuid_arg(a, "campaign_id")?).await }.await,
            "list_tags" => tags::list_tags(state, acct).await,
            "rename_tag" => async { tags::rename(state, acct, uuid_arg(a, "tag_id")?, a["name"].as_str().unwrap_or("")).await }.await,
            "delete_tag" => async {
                let id = uuid_arg(a, "tag_id")?;
                if a["confirm"].as_bool() != Some(true) {
                    let n: Option<i64> = sqlx::query_scalar("SELECT (SELECT count(*) FROM item_tags it WHERE it.tag_id = t.id) FROM tags t WHERE t.id = $1 AND t.account_id = $2").bind(id).bind(acct).fetch_optional(&state.db).await?;
                    let n = n.ok_or_else(|| AppError::not_found("tag"))?;
                    return Ok(json!({"dry_run": true, "note": format!("Nothing deleted. This takes the tag off {n} items. Call again with confirm=true once the user approves.")}));
                }
                tags::delete(state, acct, id).await?;
                audit::audit(state, ctx, "tag.delete", Some(&id.to_string()), json!({})).await;
                Ok(json!({"ok": true}))
            }
            .await,
            "tag_campaigns" | "tag_flows" | "tag_templates" => async { apply_tags(state, ctx, kind_of(name), from(a.clone())?).await }.await,
            "archive_campaigns" | "archive_flows" | "archive_templates" => async { archive(state, ctx, kind_of(name), from(a.clone())?).await }.await,
            "search_campaigns" => async {
                let s = |k: &str| a.get(k).filter(|v| !v.is_null()).map(|v| v.as_str().map(String::from).unwrap_or_else(|| v.to_string()));
                let q = tags::ListQuery { channel: s("channel"), status: s("status"), tag: s("tag"), archived: s("archived"), q: s("q"), from: s("from"), to: s("to"), limit: s("limit").or(Some("50".into())), offset: s("offset") };
                let (rows, total) = tags::campaigns(state, acct, &q).await?;
                Ok(json!({"campaigns": rows, "total": total}))
            }
            .await,
            "campaign_calendar" => async {
                let from = date(a["from"].as_str().unwrap_or(""), "from")?;
                let to = date(a["to"].as_str().unwrap_or(""), "to")?;
                calendar::calendar(state, acct, from, to, a["include_archived"].as_bool().unwrap_or(false)).await
            }
            .await,
            "reschedule_campaign" => async {
                let id = uuid_arg(a, "campaign_id")?;
                let day = date(a["date"].as_str().unwrap_or(""), "date")?;
                if a["confirm"].as_bool() != Some(true) {
                    let c = crate::campaigns::load(&state.db, acct, id).await?;
                    let (_, start) = calendar::plan_move(state, acct, &c, day).await?;
                    return Ok(json!({"dry_run": true, "from": c.send_at, "would_start": start, "note": "Nothing changed. Call again with confirm=true once the user approves."}));
                }
                reschedule(state, ctx, id, day).await
            }
            .await,
            _ => return None,
        };
        Some(r)
    }

    fn mcp_docs(&self) -> Vec<McpDoc> {
        vec![McpDoc { slug: "campaigns-plus", name: "Link tagging, resends, tags and the calendar", description: "UTM link tagging, resending to non-openers, tags, archive and search for campaigns, flows and templates, and the campaign calendar", text: DOC }]
    }
}
