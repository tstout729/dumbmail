//! Reporting: attribution settings, custom and ready-made reports, cohorts, funnels, deliverability
//! by inbox provider, benchmarks and CSV exports. `docs/REPORTS.md` is the reference.

pub mod attribution;
pub mod cohorts;
pub mod custom;
pub mod delivery;
pub mod exports;
pub mod indexes;
pub mod query;
pub mod recipients;
pub mod saved;

use async_trait::async_trait;
use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};
use uuid::Uuid;

use super::team::access::{rule, Access, Perm, RouteRule, ToolRule};
use super::{Feature, McpDoc};

use Perm::{AnalyticsRead, Export, ReportsManage};

/// Reading reports is for analysts and up (and content members, who see analytics); exports hold
/// people's data and need the export permission; saving, scheduling and deleting reports and
/// changing attribution are for managers and up. `/report-emails/{token}` is on the public list
/// in `team::access::PUBLIC_ROUTES`.
static ROUTE_RULES: &[RouteRule] = &[
    rule("GET", "/api/reports", AnalyticsRead),
    rule("POST", "/api/reports", ReportsManage),
    rule("GET", "/api/reports/catalog", AnalyticsRead),
    // Members who can get report emails: only needed to schedule one.
    rule("GET", "/api/reports/members", ReportsManage),
    rule("POST", "/api/reports/run", AnalyticsRead),
    rule("POST", "/api/reports/run.csv", AnalyticsRead),
    rule("GET", "/api/reports/attribution", AnalyticsRead),
    rule("PATCH", "/api/reports/attribution", ReportsManage),
    rule("POST", "/api/reports/attribution/preview", AnalyticsRead),
    rule("GET", "/api/reports/cohorts", AnalyticsRead),
    rule("POST", "/api/reports/funnel", AnalyticsRead),
    rule("GET", "/api/reports/deliverability", AnalyticsRead),
    rule("GET", "/api/reports/benchmarks", AnalyticsRead),
    rule("GET", "/api/reports/exports", Export),
    rule("POST", "/api/reports/exports", Export),
    rule("GET", "/api/reports/exports/{id}", Export),
    rule("GET", "/api/reports/exports/{id}/download", Export),
    rule("GET", "/api/reports/{id}", AnalyticsRead),
    rule("PATCH", "/api/reports/{id}", ReportsManage),
    rule("DELETE", "/api/reports/{id}", ReportsManage),
    rule("GET", "/api/reports/{id}/run", AnalyticsRead),
    rule("POST", "/api/reports/{id}/duplicate", ReportsManage),
];

static TOOL_RULES: &[ToolRule] = &[
    ToolRule { name: "get_report_catalog", perm: AnalyticsRead },
    ToolRule { name: "run_report", perm: AnalyticsRead },
    ToolRule { name: "list_reports", perm: AnalyticsRead },
    ToolRule { name: "get_report", perm: AnalyticsRead },
    ToolRule { name: "save_report", perm: ReportsManage },
    ToolRule { name: "duplicate_report", perm: ReportsManage },
    ToolRule { name: "delete_report", perm: ReportsManage },
    ToolRule { name: "list_report_recipients", perm: ReportsManage },
    ToolRule { name: "cohort_report", perm: AnalyticsRead },
    ToolRule { name: "funnel_report", perm: AnalyticsRead },
    ToolRule { name: "deliverability_report", perm: AnalyticsRead },
    ToolRule { name: "benchmark_report", perm: AnalyticsRead },
    ToolRule { name: "get_attribution_settings", perm: AnalyticsRead },
    ToolRule { name: "update_attribution_settings", perm: ReportsManage },
    ToolRule { name: "start_export", perm: Export },
    ToolRule { name: "list_exports", perm: Export },
    ToolRule { name: "get_export", perm: Export },
];
use crate::api::ApiJson;
use crate::auth::Ctx;
use crate::error::{AppError, AppResult};
use crate::mcp::{from, obj, tool};
use crate::state::AppState;

pub struct Module;

fn csv_response(filename: &str, body: String) -> Response {
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "text/csv; charset=utf-8".to_string()),
            (header::CONTENT_DISPOSITION, format!("attachment; filename=\"{}\"", filename.replace(['"', '\\', '\r', '\n'], ""))),
            (header::CACHE_CONTROL, "private, no-store".to_string()),
        ],
        body,
    )
        .into_response()
}

// ---------- Attribution ----------

async fn get_attribution(State(state): State<AppState>, ctx: Ctx) -> AppResult<Json<Value>> {
    Ok(Json(attribution::to_json(&attribution::load(&state.db, ctx.account_id).await?)))
}

async fn patch_attribution(State(state): State<AppState>, ctx: Ctx, ApiJson(p): ApiJson<attribution::Patch>) -> AppResult<Json<Value>> {
    let next = attribution::load(&state.db, ctx.account_id).await?.apply(p)?;
    let saved = attribution::to_json(&attribution::save(&state, ctx.account_id, &next).await?);
    crate::features::team::audit(&state, &ctx, "reports.attribution_change", None, saved.clone()).await;
    Ok(Json(saved))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PreviewBody {
    #[serde(default)]
    settings: Option<Value>,
    days: Option<i64>,
}

async fn preview_attribution(state: &AppState, account_id: Uuid, settings: Option<Value>, days: Option<i64>) -> AppResult<Value> {
    let patch: attribution::Patch = from(settings.unwrap_or(json!({})))?;
    let next = attribution::load(&state.db, account_id).await?.apply(patch)?;
    attribution::preview(state, account_id, &next, days.unwrap_or(30)).await
}

async fn preview_attribution_h(State(state): State<AppState>, ctx: Ctx, ApiJson(b): ApiJson<PreviewBody>) -> AppResult<Json<Value>> {
    Ok(Json(preview_attribution(&state, ctx.account_id, b.settings, b.days).await?))
}

// ---------- Reports ----------

async fn catalog_h(_ctx: Ctx) -> Json<Value> {
    Json(custom::catalog())
}

async fn list_h(State(state): State<AppState>, ctx: Ctx) -> AppResult<Json<Value>> {
    Ok(Json(saved::list(&state, ctx.account_id).await?))
}

async fn create_h(State(state): State<AppState>, ctx: Ctx, ApiJson(b): ApiJson<saved::Input>) -> AppResult<(StatusCode, Json<Value>)> {
    Ok((StatusCode::CREATED, Json(saved::create(&state, ctx.account_id, ctx.user_id, b).await?)))
}

async fn get_h(State(state): State<AppState>, ctx: Ctx, Path(id): Path<String>) -> AppResult<Json<Value>> {
    Ok(Json(saved::get(&state, ctx.account_id, &id).await?))
}

async fn patch_h(State(state): State<AppState>, ctx: Ctx, Path(id): Path<String>, ApiJson(b): ApiJson<saved::Input>) -> AppResult<Json<Value>> {
    Ok(Json(saved::update(&state, ctx.account_id, &id, b).await?))
}

async fn delete_h(State(state): State<AppState>, ctx: Ctx, Path(id): Path<String>) -> AppResult<StatusCode> {
    saved::delete(&state, ctx.account_id, &id).await?;
    crate::features::team::audit(&state, &ctx, "report.delete", Some(&id), json!({})).await;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct DuplicateBody {
    name: Option<String>,
}

async fn duplicate_h(State(state): State<AppState>, ctx: Ctx, Path(id): Path<String>, body: axum::body::Bytes) -> AppResult<(StatusCode, Json<Value>)> {
    let name = if body.iter().all(u8::is_ascii_whitespace) { None } else { serde_json::from_slice::<DuplicateBody>(&body)?.name };
    Ok((StatusCode::CREATED, Json(saved::duplicate(&state, ctx.account_id, ctx.user_id, &id, name).await?)))
}

async fn run_h(State(state): State<AppState>, ctx: Ctx, ApiJson(spec): ApiJson<custom::Spec>) -> AppResult<Json<Value>> {
    Ok(Json(custom::run(&state, ctx.account_id, spec, query::Mode::Request).await?))
}

async fn run_csv_h(State(state): State<AppState>, ctx: Ctx, ApiJson(spec): ApiJson<custom::Spec>) -> AppResult<Response> {
    let name = format!("report-{}.csv", spec.source);
    let result = custom::run(&state, ctx.account_id, spec, query::Mode::Request).await?;
    Ok(csv_response(&name, saved::result_csv(&result)))
}

async fn run_saved(state: &AppState, account_id: Uuid, id: &str) -> AppResult<Value> {
    let report = saved::get(state, account_id, id).await?;
    let spec: custom::Spec = serde_json::from_value(report["spec"].clone())?;
    let result = custom::run(state, account_id, spec, query::Mode::Request).await?;
    Ok(json!({"report": report, "result": result}))
}

async fn run_saved_h(State(state): State<AppState>, ctx: Ctx, Path(id): Path<String>) -> AppResult<Json<Value>> {
    Ok(Json(run_saved(&state, ctx.account_id, &id).await?))
}

async fn members_h(State(state): State<AppState>, ctx: Ctx) -> AppResult<Json<Value>> {
    Ok(Json(json!({"data": saved::members(&state, ctx.account_id).await?})))
}

// ---------- Cohorts, funnels, deliverability, benchmarks ----------

#[derive(Deserialize)]
struct CohortParams {
    by: Option<String>,
    months: Option<i64>,
    format: Option<String>,
}

async fn cohorts_h(State(state): State<AppState>, ctx: Ctx, Query(q): Query<CohortParams>) -> AppResult<Response> {
    let v = cohorts::cohorts(&state, ctx.account_id, cohorts::CohortQuery { by: q.by, months: q.months }).await?;
    if q.format.as_deref() == Some("csv") {
        return Ok(csv_response(&format!("cohorts-{}.csv", v["by"].as_str().unwrap_or("")), cohorts::cohorts_csv(&v)));
    }
    Ok(Json(v).into_response())
}

async fn funnel_h(State(state): State<AppState>, ctx: Ctx, ApiJson(q): ApiJson<cohorts::FunnelQuery>) -> AppResult<Json<Value>> {
    Ok(Json(cohorts::funnel(&state, ctx.account_id, q).await?))
}

async fn deliverability_h(State(state): State<AppState>, ctx: Ctx, Query(q): Query<delivery::DeliverabilityQuery>) -> AppResult<Json<Value>> {
    Ok(Json(delivery::report(&state, ctx.account_id, q).await?))
}

#[derive(Deserialize)]
struct BenchParams {
    industry: Option<String>,
}

async fn benchmarks_h(State(state): State<AppState>, ctx: Ctx, Query(q): Query<BenchParams>) -> AppResult<Json<Value>> {
    Ok(Json(delivery::benchmarks(&state, ctx.account_id, q.industry.as_deref()).await?))
}

// ---------- Exports ----------

async fn export_started(state: &AppState, ctx: &Ctx, v: &Value) {
    crate::features::team::audit(state, ctx, "export.start", v["id"].as_str(), json!({"kind": v["kind"]})).await;
}

async fn start_export_h(State(state): State<AppState>, ctx: Ctx, ApiJson(r): ApiJson<exports::Request>) -> AppResult<(StatusCode, Json<Value>)> {
    let v = exports::start(&state, ctx.account_id, ctx.user_id, r).await?;
    export_started(&state, &ctx, &v).await;
    Ok((StatusCode::ACCEPTED, Json(v)))
}

async fn list_exports_h(State(state): State<AppState>, ctx: Ctx) -> AppResult<Json<Value>> {
    Ok(Json(json!({"data": exports::list(&state, ctx.account_id).await?})))
}

async fn get_export_h(State(state): State<AppState>, ctx: Ctx, Path(id): Path<Uuid>) -> AppResult<Json<Value>> {
    Ok(Json(exports::get(&state, ctx.account_id, id).await?))
}

/// Stream the file chunk by chunk, so a large export never sits whole in memory.
async fn download_h(State(state): State<AppState>, ctx: Ctx, Path(id): Path<Uuid>) -> AppResult<Response> {
    let (filename, chunks, size) = exports::ready(&state, ctx.account_id, id).await?;
    let stream = futures_util::stream::unfold(0, move |seq| {
        let state = state.clone();
        async move {
            if seq >= chunks {
                return None;
            }
            let item = exports::chunk(&state, id, seq).await.map(axum::body::Bytes::from).map_err(std::io::Error::other);
            Some((item, seq + 1))
        }
    });
    Ok((
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "text/csv; charset=utf-8".to_string()),
            (header::CONTENT_DISPOSITION, format!("attachment; filename=\"{}\"", filename.replace(['"', '\\', '\r', '\n'], ""))),
            (header::CONTENT_LENGTH, size.to_string()),
            (header::CACHE_CONTROL, "private, no-store".to_string()),
        ],
        Body::from_stream(stream),
    )
        .into_response())
}

// ---------- MCP ----------

const REPORT_DOC: &str = include_str!("../../../docs/REPORTS.md");

fn spec_schema() -> Value {
    let sources: Vec<&str> = custom::SOURCES.iter().map(|s| s.key).collect();
    json!({
        "type": "object",
        "description": "Report spec. See the dumbmail://docs/reports resource (or get_report_catalog) for each source's measures, group_by and filters.",
        "properties": {
            "source": {"enum": sources},
            "measures": {"type": "array", "items": {"type": "string"}, "maxItems": 20, "description": "Default: the source's usual measures."},
            "group_by": {"type": "array", "items": {"type": "string"}, "maxItems": 2},
            "filters": {"type": "array", "maxItems": 10, "items": {"type": "object", "properties": {
                "field": {"type": "string"}, "values": {"type": "array", "items": {"type": "string"}, "minItems": 1, "maxItems": 100},
                "exclude": {"type": "boolean"}, "key": {"type": "string", "description": "Event property name, for field=property."}}, "required": ["field", "values"]}},
            "range": {"type": "object", "properties": {"days": {"type": "integer", "minimum": 1, "maximum": 731}, "start": {"type": "string", "format": "date"}, "end": {"type": "string", "format": "date"}},
                      "description": "days = the last N days up to now (default 30); or start/end dates in the account's time zone, both included."},
            "compare": {"type": "boolean", "description": "Also total the same length of time just before."},
            "metric": {"type": "string", "description": "For source=metric."},
            "property": {"type": "string", "description": "For group_by=property: the event property."},
            "sort": {"type": "object", "properties": {"by": {"type": "string"}, "desc": {"type": "boolean"}}, "required": ["by"]},
            "limit": {"type": "integer", "minimum": 1, "maximum": 1000}
        },
        "required": ["source"],
        "additionalProperties": false
    })
}

fn attribution_schema() -> Value {
    json!({
        "email_open_window_hours": {"type": "integer", "minimum": 1, "maximum": 720},
        "email_click_window_hours": {"type": "integer", "minimum": 1, "maximum": 720},
        "sms_click_window_hours": {"type": "integer", "minimum": 1, "maximum": 720},
        "count_opens": {"type": "boolean", "description": "false = clicks only (Apple Mail opens many emails by itself)."},
        "model": {"enum": ["last_touch", "first_touch"]},
        "conversion_metrics": {"type": "array", "items": {"type": "string"}, "minItems": 1, "maxItems": 5}
    })
}

fn export_schema() -> Value {
    json!({
        "kind": {"enum": exports::KINDS},
        "scope": {"enum": ["all", "list", "segment"], "description": "profiles only (default all)."},
        "list_id": {"type": "string"}, "segment_id": {"type": "string"},
        "columns": {"type": "array", "items": {"enum": exports::PROFILE_COLUMNS.iter().map(|(k, _)| *k).collect::<Vec<_>>()}, "description": "profiles only."},
        "properties": {"type": "array", "items": {"type": "string"}, "maxItems": 50, "description": "profiles only: custom properties to add as columns."},
        "metric": {"type": "string", "description": "events only."},
        "start": {"type": "string", "format": "date"}, "end": {"type": "string", "format": "date"},
        "campaign_id": {"type": "string", "description": "campaign_recipients only."}
    })
}

fn tools() -> Vec<Value> {
    let id = json!({"type": "string", "description": "A saved report's id, or builtin:<key> for a ready-made one."});
    vec![
        tool("get_report_catalog", "Report building blocks", "Every report source with its measures, group_by options and filters.", obj(json!({}), &[]), true, false),
        tool("run_report", "Run a report", "Run an ad-hoc report spec and get rows, totals and (with compare) the previous period. Read the dumbmail://docs/reports resource for the spec format.", obj(json!({"spec": spec_schema()}), &["spec"]), true, false),
        tool("list_reports", "List reports", "Ready-made reports (ids builtin:<key>) and the account's saved reports with their email schedules.", obj(json!({}), &[]), true, false),
        tool("get_report", "Get and run a report", "A saved or ready-made report's definition and its current results.", obj(json!({"id": id}), &["id"]), true, false),
        tool("save_report", "Save a report", "Create a saved report, or update one by id. A schedule (weekly on Mondays or monthly on the 1st, 8 a.m. account time) emails a summary and a CSV link to the chosen members (user ids from list_report_recipients). Each member gets the emails only after confirming their address from a link we email them (members who sign in with Google count as confirmed); recipient_status shows who is still waiting. Setting or changing a schedule requires confirm=true and the user's approval.",
            obj(json!({"id": {"type": "string"}, "name": {"type": "string", "maxLength": 120}, "description": {"type": "string", "maxLength": 500}, "spec": spec_schema(),
                       "schedule": {"enum": ["weekly", "monthly", null]}, "recipients": {"type": "array", "items": {"type": "string"}, "maxItems": 10}, "confirm": {"type": "boolean"}}), &[]), false, false),
        tool("duplicate_report", "Duplicate a report", "Copy a saved or ready-made report into a new saved report you can change.", obj(json!({"id": id, "name": {"type": "string"}}), &["id"]), false, false),
        tool("delete_report", "Delete a report", "Delete a saved report and its schedule. Without confirm=true it only describes what would happen.", obj(json!({"id": {"type": "string"}, "confirm": {"type": "boolean"}}), &["id"]), false, true),
        tool("list_report_recipients", "Members for report emails", "Members of the account who can get scheduled report emails, each with report_emails: confirmed, waiting (a confirmation link was sent) or not_asked.", obj(json!({}), &[]), true, false),
        tool("cohort_report", "Cohort retention", "Group people by first purchase month (or sign-up month) and show, for each month since, the share who bought (again) and cumulative revenue per person.",
            obj(json!({"by": {"enum": ["first_purchase", "signup"]}, "months": {"type": "integer", "minimum": 1, "maximum": 24}}), &[]), true, false),
        tool("funnel_report", "Funnel", "Ordered steps from any metrics (e.g. Viewed Product, Added to Cart, Started Checkout, Placed Order) within a window from the first step, with conversion and drop-off per step, optionally split by an event or profile property or by whether the first step came from a campaign or flow.",
            obj(json!({"steps": {"type": "array", "items": {"type": "string"}, "minItems": 2, "maxItems": 6}, "window_days": {"type": "integer", "minimum": 1, "maximum": 90},
                       "range": {"type": "object", "properties": {"days": {"type": "integer"}, "start": {"type": "string", "format": "date"}, "end": {"type": "string", "format": "date"}}},
                       "split_by": {"type": "object", "properties": {"type": {"enum": ["event_property", "profile_property", "source"]}, "key": {"type": "string"}}, "required": ["type"]}}), &["steps"]), true, false),
        tool("deliverability_report", "Deliverability by inbox provider", "Email sent, delivered, open, click, bounce (hard/soft), complaint and unsubscribe rates by inbox provider (Gmail, Outlook, Yahoo/AOL, Apple, other) and sending domain, over time, with a health score, plain-language flags and DKIM/SPF/DMARC status.",
            obj(json!({"days": {"type": "integer", "minimum": 1, "maximum": 365}, "interval": {"enum": ["day", "week"]}}), &[]), true, false),
        tool("benchmark_report", "Benchmarks", "The account's email rates over the last 30 days next to its last 90 days and published industry figures (source and date included).", obj(json!({"industry": {"type": "string", "description": "Industry key from the result's industries list (default all)."}}), &[]), true, false),
        tool("get_attribution_settings", "Attribution settings", "How revenue is credited to messages: email open and click windows, SMS click window, whether opens count, last or first touch, and which metrics count as conversions.", obj(json!({}), &[]), true, false),
        tool("update_attribution_settings", "Change attribution settings", "Change how revenue is credited. Every report (overview, campaigns, flows, custom) uses it from then on, including past dates. With preview=true nothing is saved and you get the last `days` of revenue under the current and the proposed settings.",
            obj({
                let mut p = attribution_schema();
                p["preview"] = json!({"type": "boolean"});
                p["days"] = json!({"type": "integer", "minimum": 1, "maximum": 365, "description": "Preview period (default 30)."});
                p
            }, &[]), false, false),
        tool("start_export", "Export CSV", "Start a CSV export: profiles (all, a list or a segment; chosen columns, consent and custom properties), events of a metric in a date range, a campaign's recipients with each one's outcome, or unsubscribes and suppressions. Runs in the background; one at a time per account. Files are kept 7 days and download only while signed in.",
            obj(export_schema(), &["kind"]), false, false),
        tool("list_exports", "List exports", "Recent exports and their status.", obj(json!({}), &[]), true, false),
        tool("get_export", "Get an export", "An export's status, size and row count. Download it from the dashboard (Analytics → Exports) or GET download_url with the API key.", obj(json!({"id": {"type": "string"}}), &["id"]), true, false),
    ]
}

async fn call(state: &AppState, ctx: &Ctx, name: &str, a: &Value) -> AppResult<Value> {
    let acct = ctx.account_id;
    let str_arg = |k: &str| a[k].as_str().map(str::to_string);
    match name {
        "get_report_catalog" => Ok(custom::catalog()),
        "run_report" => custom::run(state, acct, from(a["spec"].clone())?, query::Mode::Request).await,
        "list_reports" => saved::list(state, acct).await,
        "get_report" => run_saved(state, acct, &str_arg("id").unwrap_or_default()).await,
        "save_report" => {
            let mut body = a.clone();
            let confirmed = body.get("confirm").and_then(Value::as_bool) == Some(true);
            let id = body.get("id").and_then(Value::as_str).map(str::to_string);
            if let Some(o) = body.as_object_mut() {
                o.remove("confirm");
                o.remove("id");
            }
            let input: saved::Input = from(body)?;
            let schedule_set = matches!(&input.schedule, Some(Some(_))) || input.recipients.is_some();
            if schedule_set && !confirmed {
                return Ok(json!({"dry_run": true, "note": format!(
                    "Nothing saved. This would email the report {} to {} member(s), once each confirms their address from the link we email them. Call again with confirm=true once the user approves.",
                    input.schedule.clone().flatten().unwrap_or_else(|| "on its current schedule".into()),
                    input.recipients.as_ref().map_or(0, Vec::len))}));
            }
            match id {
                Some(id) => saved::update(state, acct, &id, input).await,
                None => saved::create(state, acct, ctx.user_id, input).await,
            }
        }
        "duplicate_report" => saved::duplicate(state, acct, ctx.user_id, &str_arg("id").unwrap_or_default(), str_arg("name")).await,
        "delete_report" => {
            let id = str_arg("id").unwrap_or_default();
            let report = saved::get(state, acct, &id).await?;
            if a["confirm"].as_bool() != Some(true) {
                return Ok(json!({"dry_run": true, "note": format!("Nothing deleted. This deletes the saved report {:?}{}. Call again with confirm=true once the user approves.",
                    report["name"].as_str().unwrap_or(""), if report["schedule"].is_string() { " and stops its scheduled emails" } else { "" })}));
            }
            saved::delete(state, acct, &id).await?;
            crate::features::team::audit(state, ctx, "report.delete", Some(&id), json!({})).await;
            Ok(json!({"deleted": true}))
        }
        "list_report_recipients" => Ok(json!({"data": saved::members(state, acct).await?})),
        "cohort_report" => cohorts::cohorts(state, acct, from(a.clone())?).await,
        "funnel_report" => cohorts::funnel(state, acct, from(a.clone())?).await,
        "deliverability_report" => delivery::report(state, acct, from(a.clone())?).await,
        "benchmark_report" => delivery::benchmarks(state, acct, a["industry"].as_str()).await,
        "get_attribution_settings" => Ok(attribution::to_json(&attribution::load(&state.db, acct).await?)),
        "update_attribution_settings" => {
            let mut body = a.clone();
            let preview = body.get("preview").and_then(Value::as_bool) == Some(true);
            let days = body.get("days").and_then(Value::as_i64);
            if let Some(o) = body.as_object_mut() {
                o.remove("preview");
                o.remove("days");
            }
            if preview {
                return preview_attribution(state, acct, Some(body), days).await;
            }
            let next = attribution::load(&state.db, acct).await?.apply(from(body)?)?;
            let saved = attribution::to_json(&attribution::save(state, acct, &next).await?);
            crate::features::team::audit(state, ctx, "reports.attribution_change", None, saved.clone()).await;
            Ok(saved)
        }
        "start_export" => {
            let v = exports::start(state, acct, ctx.user_id, from(a.clone())?).await?;
            export_started(state, ctx, &v).await;
            Ok(v)
        }
        "list_exports" => Ok(json!({"data": exports::list(state, acct).await?})),
        "get_export" => exports::get(state, acct, str_arg("id").unwrap_or_default().parse().map_err(|_| AppError::not_found("export"))?).await,
        _ => Err(AppError::not_found("tool")),
    }
}

const TOOL_NAMES: &[&str] = &[
    "get_report_catalog", "run_report", "list_reports", "get_report", "save_report", "duplicate_report", "delete_report", "list_report_recipients", "cohort_report",
    "funnel_report", "deliverability_report", "benchmark_report", "get_attribution_settings", "update_attribution_settings", "start_export", "list_exports", "get_export",
];

#[async_trait]
impl Feature for Module {
    fn name(&self) -> &'static str {
        "reports"
    }

    fn routes(&self) -> Router<AppState> {
        recipients::routes()
            .route("/api/reports", get(list_h).post(create_h))
            .route("/api/reports/catalog", get(catalog_h))
            .route("/api/reports/members", get(members_h))
            .route("/api/reports/run", post(run_h))
            .route("/api/reports/run.csv", post(run_csv_h))
            .route("/api/reports/attribution", get(get_attribution).patch(patch_attribution))
            .route("/api/reports/attribution/preview", post(preview_attribution_h))
            .route("/api/reports/cohorts", get(cohorts_h))
            .route("/api/reports/funnel", post(funnel_h))
            .route("/api/reports/deliverability", get(deliverability_h))
            .route("/api/reports/benchmarks", get(benchmarks_h))
            .route("/api/reports/exports", get(list_exports_h).post(start_export_h))
            .route("/api/reports/exports/{id}", get(get_export_h))
            .route("/api/reports/exports/{id}/download", get(download_h))
            .route("/api/reports/{id}", get(get_h).patch(patch_h).delete(delete_h))
            .route("/api/reports/{id}/run", get(run_saved_h))
            .route("/api/reports/{id}/duplicate", post(duplicate_h))
    }

    fn mcp_tools(&self) -> Vec<Value> {
        tools()
    }

    fn access(&self) -> Access {
        Access { routes: ROUTE_RULES, tools: TOOL_RULES }
    }

    async fn mcp_call(&self, state: &AppState, ctx: &Ctx, name: &str, args: &Value) -> Option<AppResult<Value>> {
        if !TOOL_NAMES.contains(&name) {
            return None;
        }
        Some(call(state, ctx, name, args).await)
    }

    fn mcp_docs(&self) -> Vec<McpDoc> {
        vec![McpDoc {
            slug: "reports",
            name: "Reports reference",
            description: "Report spec format (sources, measures, group_by, filters, ranges) with examples, attribution settings, cohorts, funnels, deliverability, benchmarks and exports",
            text: REPORT_DOC,
        }]
    }

    async fn run_job(&self, state: &AppState, kind: &str, payload: &Value) -> Option<anyhow::Result<()>> {
        let id = |k: &str| payload[k].as_str().and_then(|s| s.parse::<Uuid>().ok()).ok_or_else(|| anyhow::anyhow!("job payload missing {k}"));
        match kind {
            "reports_export" => Some(match id("export_id") {
                Ok(i) => exports::run_batch(state, i).await,
                Err(e) => Err(e),
            }),
            "reports_scheduled_send" => {
                let occurrence = payload["occurrence"].as_str().and_then(|s| s.parse::<chrono::DateTime<chrono::Utc>>().ok());
                Some(match (id("report_id"), occurrence) {
                    (Ok(i), Some(o)) => saved::send_scheduled(state, i, o).await,
                    (Err(e), _) => Err(e),
                    (_, None) => Err(anyhow::anyhow!("job payload missing occurrence")),
                })
            }
            "reports_mx_lookup" => Some(match id("account_id") {
                Ok(i) => delivery::mx_lookup(state, i).await,
                Err(e) => Err(e),
            }),
            _ => None,
        }
    }

    async fn after_start(&self, state: &AppState) -> anyhow::Result<()> {
        indexes::ensure(state).await
    }

    async fn tick(&self, state: &AppState, tick: u64) -> anyhow::Result<()> {
        if tick.is_multiple_of(3) {
            saved::enqueue_due(state).await?;
        }
        if tick.is_multiple_of(180) {
            exports::prune(state).await?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn every_tool_is_dispatched() {
        let names: Vec<String> = super::tools().iter().map(|t| t["name"].as_str().unwrap().to_string()).collect();
        assert_eq!(names.len(), super::TOOL_NAMES.len());
        for n in names {
            assert!(super::TOOL_NAMES.contains(&n.as_str()), "{n}");
        }
    }
}
