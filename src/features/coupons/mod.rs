//! Unique coupon codes: pools of single-use codes, given one per person through the
//! `{% coupon_code 'NAME' %}` tag, the email builder's coupon block and sign-up forms, with
//! redemptions learned from orders. `docs/COUPONS.md` is the reference.

pub mod assign;
pub mod checks;
pub mod redeem;
pub mod store;
pub mod template;

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

use super::{Feature, McpDoc};
use crate::api::{ApiJson, Page};
use crate::auth::Ctx;
use crate::error::{AppError, AppResult};
use crate::mcp::{from, obj, tool};
use crate::state::AppState;

pub use assign::{give_code, CreateInStore, Given, Prepared, SendCoupons, StoreCodeFuture, Via};

pub struct Module;

// ---------- REST ----------

async fn list_h(State(state): State<AppState>, ctx: Ctx, Query(p): Query<Page>) -> AppResult<Json<Value>> {
    let (rows, total) = store::list(&state.db, ctx.account_id, p.limit().min(100), p.offset()).await?;
    Ok(Json(json!({"data": rows, "total": total})))
}

async fn create_h(State(state): State<AppState>, ctx: Ctx, ApiJson(b): ApiJson<store::Input>) -> AppResult<(StatusCode, Json<Value>)> {
    let c = store::create(&state.db, ctx.account_id, b).await?;
    Ok((StatusCode::CREATED, Json(store::to_json(&state.db, &c).await?)))
}

async fn get_h(State(state): State<AppState>, ctx: Ctx, Path(id): Path<Uuid>) -> AppResult<Json<Value>> {
    let c = store::load(&state.db, ctx.account_id, id).await?;
    Ok(Json(store::to_json(&state.db, &c).await?))
}

async fn patch_h(State(state): State<AppState>, ctx: Ctx, Path(id): Path<Uuid>, ApiJson(b): ApiJson<store::Input>) -> AppResult<Json<Value>> {
    let c = store::update(&state.db, ctx.account_id, id, b).await?;
    Ok(Json(store::to_json(&state.db, &c).await?))
}

async fn delete_coupon(state: &AppState, account_id: Uuid, id: Uuid) -> AppResult<()> {
    let c = store::load(&state.db, account_id, id).await?;
    let users = store::users(&state.db, &c).await?;
    if !users.is_empty() {
        return Err(AppError::Conflict(format!("coupon {:?} is in use by {}; stop or change those first", c.name, users.join(", "))));
    }
    store::delete(&state.db, &c).await
}

async fn delete_h(State(state): State<AppState>, ctx: Ctx, Path(id): Path<Uuid>) -> AppResult<StatusCode> {
    delete_coupon(&state, ctx.account_id, id).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn codes_h(State(state): State<AppState>, ctx: Ctx, Path(id): Path<Uuid>, Query(q): Query<store::CodeQuery>) -> AppResult<Json<Value>> {
    let c = store::load(&state.db, ctx.account_id, id).await?;
    Ok(Json(store::codes(&state.db, &c, &q).await?))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AddBody {
    /// One code per item.
    codes: Option<Vec<String>>,
    /// Pasted text or a CSV file's contents: one code per line, first column.
    text: Option<String>,
}

async fn add_codes(state: &AppState, account_id: Uuid, id: Uuid, b: AddBody) -> AppResult<Value> {
    let c = store::load(&state.db, account_id, id).await?;
    let (codes, refused) = match (b.codes, b.text) {
        (Some(list), None) => {
            if list.len() > store::MAX_PER_UPLOAD {
                return Err(AppError::bad(format!("upload at most {} codes at a time", store::MAX_PER_UPLOAD)));
            }
            let n = list.len();
            let ok: Vec<String> = list.into_iter().map(|s| s.trim().to_string()).filter(|s| store::valid_code(s)).collect();
            let refused = n - ok.len();
            (ok, refused)
        }
        (None, Some(text)) => store::parse_codes(&text)?,
        _ => return Err(AppError::bad("send either codes (a list) or text (pasted codes or a CSV)")),
    };
    if codes.is_empty() {
        return Err(AppError::bad("no usable codes found: one code per line, 1-100 characters, no spaces"));
    }
    let (added, duplicates) = store::add_codes(&state.db, &c, codes).await?;
    let c = store::load(&state.db, account_id, id).await?;
    Ok(json!({"added": added, "duplicates": duplicates, "refused": refused, "coupon": store::to_json(&state.db, &c).await?}))
}

async fn add_codes_h(State(state): State<AppState>, ctx: Ctx, Path(id): Path<Uuid>, ApiJson(b): ApiJson<AddBody>) -> AppResult<Json<Value>> {
    Ok(Json(add_codes(&state, ctx.account_id, id, b).await?))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GenerateBody {
    count: usize,
    pattern: Option<store::Pattern>,
}

async fn generate(state: &AppState, account_id: Uuid, id: Uuid, b: GenerateBody) -> AppResult<Value> {
    let c = store::load(&state.db, account_id, id).await?;
    let added = store::generate(&state.db, &c, b.count, b.pattern).await?;
    let c = store::load(&state.db, account_id, id).await?;
    Ok(json!({"added": added, "coupon": store::to_json(&state.db, &c).await?}))
}

async fn generate_h(State(state): State<AppState>, ctx: Ctx, Path(id): Path<Uuid>, ApiJson(b): ApiJson<GenerateBody>) -> AppResult<Json<Value>> {
    Ok(Json(generate(&state, ctx.account_id, id, b).await?))
}

#[derive(Deserialize)]
struct ExportQuery {
    status: Option<String>,
}

/// CSV of the coupon's codes, streamed a chunk at a time (a pool can hold 2 million).
async fn export_h(State(state): State<AppState>, ctx: Ctx, Path(id): Path<Uuid>, Query(q): Query<ExportQuery>) -> AppResult<Response> {
    let c = store::load(&state.db, ctx.account_id, id).await?;
    let status = q.status.filter(|s| !s.is_empty() && s != "all");
    if status.as_deref().is_some_and(|s| !matches!(s, "available" | "assigned" | "redeemed")) {
        return Err(AppError::bad("status must be available, assigned, redeemed or all"));
    }
    let filename = format!("coupon-{}-{}.csv", c.name.replace(|ch: char| !ch.is_ascii_alphanumeric(), "-"), status.as_deref().unwrap_or("all"));
    let stream = futures_util::stream::unfold(Some((0i64, true)), move |cursor| {
        let (state, c, status) = (state.clone(), c.clone(), status.clone());
        async move {
            let (after, header) = cursor?;
            match store::export_chunk(&state.db, &c, status.as_deref(), after, header).await {
                Ok(Some((text, last))) => {
                    let next = if last == after { None } else { Some((last, false)) };
                    Some((Ok::<_, std::io::Error>(axum::body::Bytes::from(text)), next))
                }
                Ok(None) => None,
                Err(e) => Some((Err(std::io::Error::other(e.public_message())), None)),
            }
        }
    });
    Ok((
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "text/csv; charset=utf-8".to_string()),
            (header::CONTENT_DISPOSITION, format!("attachment; filename=\"{filename}\"")),
            (header::CACHE_CONTROL, "private, no-store".to_string()),
        ],
        Body::from_stream(stream),
    )
        .into_response())
}

#[derive(Deserialize)]
struct ReportQuery {
    days: Option<i64>,
}

async fn report_h(State(state): State<AppState>, ctx: Ctx, Path(id): Path<Uuid>, Query(q): Query<ReportQuery>) -> AppResult<Json<Value>> {
    let c = store::load(&state.db, ctx.account_id, id).await?;
    Ok(Json(redeem::report(&state.db, &c, q.days.unwrap_or(30)).await?))
}

async fn summary_h(State(state): State<AppState>, ctx: Ctx) -> AppResult<Json<Value>> {
    Ok(Json(redeem::summary(&state.db, ctx.account_id).await?))
}

// ---------- MCP ----------

const COUPONS_DOC: &str = include_str!("../../../docs/COUPONS.md");

const TOOL_NAMES: &[&str] = &["list_coupons", "get_coupon", "create_coupon", "update_coupon", "add_coupon_codes", "generate_coupon_codes", "delete_coupon", "coupon_report"];

fn settings_schema() -> Value {
    json!({
        "description": {"type": "string", "maxLength": 500},
        "source": {"enum": ["uploaded", "generated"], "description": "uploaded: codes made in your store. generated: DumbMail makes them from pattern (export them and import into the store)."},
        "pattern": {"type": ["object", "null"], "properties": {
            "prefix": {"type": "string", "maxLength": 20, "description": "Letters, digits, - and _."},
            "length": {"type": "integer", "minimum": 4, "maximum": 24, "description": "Random characters after the prefix."},
            "alphabet": {"enum": ["letters_digits", "letters", "digits"], "description": "letters_digits (default) leaves out look-alikes 0, O, 1, I and L."}},
            "required": ["length"], "additionalProperties": false},
        "expires_at": {"type": ["string", "null"], "format": "date-time", "description": "Codes stop working on this date."},
        "expires_after_days": {"type": ["integer", "null"], "minimum": 1, "maximum": 3650, "description": "Or: each code expires this many days after it's given out."},
        "new_code_each_time": {"type": "boolean", "description": "false (default): a person gets the same code in every message. true: a new code in every message."},
        "low_stock_threshold": {"type": ["integer", "null"], "minimum": 1, "maximum": 1000000, "description": "Alert when fewer codes than this are left."},
        "form_codes_per_hour": {"type": "integer", "minimum": 1, "maximum": 10000, "description": "Most codes sign-up forms may hand out per hour (default 200)."}
    })
}

fn tools() -> Vec<Value> {
    let id = json!({"type": "string", "description": "The coupon's id or name."});
    let mut create = settings_schema();
    create["name"] = json!({"type": "string", "maxLength": 64, "description": "What templates use: {% coupon_code 'NAME' %}. Letters, digits, spaces, . _ -. Can't change later."});
    let mut update = settings_schema();
    update["id"] = id.clone();
    vec![
        tool("list_coupons", "List coupons", "Coupons with their stock (total, available, assigned, redeemed), alerts (out of codes, low stock, campaigns and flows waiting for codes) and the template tag to use.",
            obj(json!({"limit": {"type": "integer", "minimum": 1, "maximum": 100}, "offset": {"type": "integer", "minimum": 0}}), &[]), true, false),
        tool("get_coupon", "Get a coupon", "One coupon's settings, stock and alerts, plus a page of its codes (search by the start of a code, or by a person's email).",
            obj(json!({"id": id, "search": {"type": "string", "description": "Start of a code, or an email address."}, "status": {"enum": ["available", "assigned", "redeemed", "all"]},
                       "limit": {"type": "integer", "minimum": 1, "maximum": 200}, "offset": {"type": "integer", "minimum": 0, "maximum": 10000}}), &["id"]), true, false),
        tool("create_coupon", "Create a coupon", "Create a pool of unique single-use codes. Then add codes with add_coupon_codes (made in your store) or generate_coupon_codes, and use {% coupon_code 'NAME' %} or a coupon block in emails and texts, or set a sign-up form's config.coupon.",
            obj(create, &["name"]), false, false),
        tool("update_coupon", "Change a coupon", "Change a coupon's description, expiry, low-stock alert, per-message mode or form limit. The name can't change.", obj(update, &["id"]), false, false),
        tool("add_coupon_codes", "Add coupon codes", "Add codes made in your store: a list, or pasted text / CSV (one per line, first column, a header row is skipped). Up to 200,000 per call and 2,000,000 per coupon. Codes already in the coupon (any letter case) are skipped. Adding codes resumes campaigns paused because the coupon ran out.",
            obj(json!({"id": id, "codes": {"type": "array", "items": {"type": "string", "maxLength": 100}, "maxItems": 200000}, "text": {"type": "string"}}), &["id"]), false, false),
        tool("generate_coupon_codes", "Generate coupon codes", "Make new random codes from the coupon's pattern (or the one given, which is saved). The store must accept them: export them from the dashboard (Content → Coupons → Export) and import them into the store before sending.",
            obj(json!({"id": id, "count": {"type": "integer", "minimum": 1, "maximum": 200000}, "pattern": settings_schema()["pattern"]}), &["id", "count"]), false, false),
        tool("delete_coupon", "Delete a coupon", "Delete a coupon and all its codes, including who got which code and redemptions. Refused while a scheduled or sending campaign, a live or paused flow, or a live form uses it. Without confirm=true it only describes what would happen; get the user's approval first.",
            obj(json!({"id": id, "confirm": {"type": "boolean"}}), &["id"]), false, true),
        tool("coupon_report", "Coupon report", "Codes given, redeemed, redemption rate and revenue: for one coupon (with a daily breakdown over `days`), or every coupon when id is left out. Orders recorded by the website script count as unverified and add no revenue.",
            obj(json!({"id": id, "days": {"type": "integer", "minimum": 1, "maximum": 365}}), &[]), true, false),
    ]
}

async fn call(state: &AppState, ctx: &Ctx, name: &str, a: &Value) -> AppResult<Value> {
    let acct = ctx.account_id;
    let key = || a["id"].as_str().map(str::to_string).ok_or_else(|| AppError::bad("id is required"));
    match name {
        "list_coupons" => {
            let (rows, total) = store::list(&state.db, acct, a["limit"].as_i64().unwrap_or(50).clamp(1, 100), a["offset"].as_i64().unwrap_or(0).max(0)).await?;
            Ok(json!({"data": rows, "total": total}))
        }
        "get_coupon" => {
            let c = store::find(&state.db, acct, &key()?).await?;
            let q = store::CodeQuery { q: a["search"].as_str().map(str::to_string), status: a["status"].as_str().map(str::to_string), limit: a["limit"].as_i64(), offset: a["offset"].as_i64() };
            let mut v = store::to_json(&state.db, &c).await?;
            v["codes"] = store::codes(&state.db, &c, &q).await?;
            Ok(v)
        }
        "create_coupon" => {
            let c = store::create(&state.db, acct, from(a.clone())?).await?;
            store::to_json(&state.db, &c).await
        }
        "update_coupon" => {
            let c = store::find(&state.db, acct, &key()?).await?;
            let mut body = a.clone();
            if let Some(o) = body.as_object_mut() {
                o.remove("id");
            }
            let c = store::update(&state.db, acct, c.id, from(body)?).await?;
            store::to_json(&state.db, &c).await
        }
        "add_coupon_codes" => {
            let c = store::find(&state.db, acct, &key()?).await?;
            let body = AddBody { codes: from(a["codes"].clone())?, text: a["text"].as_str().map(str::to_string) };
            add_codes(state, acct, c.id, body).await
        }
        "generate_coupon_codes" => {
            let c = store::find(&state.db, acct, &key()?).await?;
            let count = a["count"].as_u64().ok_or_else(|| AppError::bad("count is required"))? as usize;
            let pattern = if a["pattern"].is_null() { None } else { Some(from(a["pattern"].clone())?) };
            generate(state, acct, c.id, GenerateBody { count, pattern }).await
        }
        "delete_coupon" => {
            let c = store::find(&state.db, acct, &key()?).await?;
            if a["confirm"].as_bool() != Some(true) {
                let stock = store::stock(&state.db, &c).await?;
                let users = store::users(&state.db, &c).await?;
                let note = if users.is_empty() {
                    format!(
                        "Nothing deleted. This permanently deletes coupon {:?} and its {} codes, including {} given to people and {} redeemed, and its report. Call again with confirm=true once the user approves.",
                        c.name, stock["total"], stock["assigned"], stock["redeemed"]
                    )
                } else {
                    format!("Nothing deleted, and it can't be yet: coupon {:?} is in use by {}.", c.name, users.join(", "))
                };
                return Ok(json!({"dry_run": true, "note": note}));
            }
            delete_coupon(state, acct, c.id).await?;
            Ok(json!({"deleted": true}))
        }
        "coupon_report" => match a["id"].as_str() {
            Some(k) => {
                let c = store::find(&state.db, acct, k).await?;
                redeem::report(&state.db, &c, a["days"].as_i64().unwrap_or(30)).await
            }
            None => redeem::summary(&state.db, acct).await,
        },
        _ => Err(AppError::bad("unknown tool")),
    }
}

#[async_trait]
impl Feature for Module {
    fn name(&self) -> &'static str {
        "coupons"
    }

    fn routes(&self) -> Router<AppState> {
        Router::new()
            .route("/api/coupons", get(list_h).post(create_h))
            .route("/api/coupons/report", get(summary_h))
            .route("/api/coupons/{id}", get(get_h).patch(patch_h).delete(delete_h))
            // 200,000 codes of up to 100 characters, as JSON.
            .route("/api/coupons/{id}/codes", get(codes_h).post(add_codes_h).layer(axum::extract::DefaultBodyLimit::max(24 * 1024 * 1024)))
            .route("/api/coupons/{id}/generate", post(generate_h))
            .route("/api/coupons/{id}/export", get(export_h))
            .route("/api/coupons/{id}/report", get(report_h))
    }

    fn mcp_tools(&self) -> Vec<Value> {
        tools()
    }

    async fn mcp_call(&self, state: &AppState, ctx: &Ctx, name: &str, args: &Value) -> Option<AppResult<Value>> {
        if !TOOL_NAMES.contains(&name) {
            return None;
        }
        Some(call(state, ctx, name, args).await)
    }

    fn mcp_docs(&self) -> Vec<McpDoc> {
        vec![McpDoc {
            slug: "coupons",
            name: "Unique coupon codes",
            description: "Coupons (pools of single-use codes), the {% coupon_code %} tag and coupon block, sign-up form codes, redemptions and limits.",
            text: COUPONS_DOC,
        }]
    }

    async fn run_job(&self, state: &AppState, kind: &str, payload: &Value) -> Option<anyhow::Result<()>> {
        let id = || payload["coupon_id"].as_str().and_then(|s| s.parse::<Uuid>().ok()).ok_or_else(|| anyhow::anyhow!("job payload missing coupon_id"));
        match kind {
            "coupons_alert" => Some(match id() {
                Ok(i) => assign::send_alert(state, i, payload["kind"].as_str().unwrap_or("empty"), payload["left"].as_i64()).await,
                Err(e) => Err(e),
            }),
            "coupons_purge" => Some(match id() {
                Ok(i) => store::purge(&state.db, i).await,
                Err(e) => Err(e),
            }),
            _ => None,
        }
    }

    async fn tick(&self, state: &AppState, tick: u64) -> anyhow::Result<()> {
        if tick.is_multiple_of(3) {
            let n = assign::release_unsent(&state.db).await?;
            if n > 0 {
                tracing::info!(released = n, "coupon codes from unsent messages returned to their pools");
            }
            assign::check_low_stock(&state.db).await?;
        }
        Ok(())
    }

    async fn wants_event(&self, db: &sqlx::PgPool, account_id: Uuid, metric: &str) -> bool {
        metric.trim().eq_ignore_ascii_case(redeem::ORDER_METRIC) && redeem::account_has_coupons(db, account_id).await
    }

    async fn on_event(&self, state: &AppState, event_id: Uuid) -> anyhow::Result<()> {
        redeem::on_order(&state.db, event_id).await.map(|_| ())
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
