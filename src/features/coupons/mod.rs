//! Unique coupon codes: pools of single-use codes, given one per person through the
//! `{% coupon_code 'NAME' %}` tag, the email builder's coupon block and sign-up forms, with
//! redemptions matched from orders. `docs/COUPONS.md` is the reference.

pub mod assign;
pub mod checks;
pub mod redeem;
pub mod store;
pub mod template;

use async_trait::async_trait;
use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures_util::StreamExt;
use serde::Deserialize;
use serde_json::{json, Value};
use uuid::Uuid;

use super::team::access::{require, rule, Access, Perm, RouteRule, ToolRule};
use super::team::audit;
use super::{Feature, McpDoc};
use crate::api::{ApiJson, Page};
use crate::auth::Ctx;
use crate::error::{AppError, AppResult};
use crate::mcp::{from, obj, tool};
use crate::state::AppState;

pub use assign::{give_code, CreateInStore, Given, Prepared, SendCoupons, StoreCodeFuture, Via};

pub struct Module;

// Who may do what (docs/COUPONS.md, "Permissions"). Reading coupons, stock and reports: anyone who
// reads content. Raw codes, who holds them and exports: people who manage coupons (catalog.write,
// Managers and up), who must also be able to read profiles (checked in [`raw_codes_allowed`]).
// Creating, editing, adding and generating: catalog.write. Deleting: catalog.delete (Admins and
// the Owner), with confirm=true. Sign-up form codes are handed out by the public form routes.
static ROUTES: &[RouteRule] = &[
    rule("GET", "/api/coupons", Perm::ContentRead),
    rule("POST", "/api/coupons", Perm::CatalogWrite),
    rule("GET", "/api/coupons/report", Perm::ContentRead),
    rule("GET", "/api/coupons/campaign-check/{campaign_id}", Perm::MessagingRead),
    rule("GET", "/api/coupons/{id}", Perm::ContentRead),
    rule("PATCH", "/api/coupons/{id}", Perm::CatalogWrite),
    rule("DELETE", "/api/coupons/{id}", Perm::CatalogDelete),
    rule("GET", "/api/coupons/{id}/codes", Perm::CatalogWrite),
    rule("POST", "/api/coupons/{id}/codes", Perm::CatalogWrite),
    rule("POST", "/api/coupons/{id}/generate", Perm::CatalogWrite),
    rule("GET", "/api/coupons/{id}/export", Perm::CatalogWrite),
    rule("GET", "/api/coupons/{id}/report", Perm::ContentRead),
];

static TOOLS: &[ToolRule] = &[
    ToolRule { name: "list_coupons", perm: Perm::ContentRead },
    ToolRule { name: "coupon_report", perm: Perm::ContentRead },
    ToolRule { name: "get_coupon", perm: Perm::CatalogWrite },
    ToolRule { name: "export_coupon_codes", perm: Perm::CatalogWrite },
    ToolRule { name: "create_coupon", perm: Perm::CatalogWrite },
    ToolRule { name: "update_coupon", perm: Perm::CatalogWrite },
    ToolRule { name: "add_coupon_codes", perm: Perm::CatalogWrite },
    ToolRule { name: "generate_coupon_codes", perm: Perm::CatalogWrite },
    ToolRule { name: "delete_coupon", perm: Perm::CatalogDelete },
];

/// Raw codes come with who holds them (people's emails): also needs profile access.
fn raw_codes_allowed(ctx: &Ctx) -> AppResult<()> {
    require(ctx, Perm::ProfilesRead)
}

/// Largest JSON body for adding codes (bigger uploads send the file itself, streamed).
const MAX_JSON_UPLOAD: usize = 2 * 1024 * 1024;
/// Most codes one MCP export call returns.
const MCP_EXPORT_MAX: i64 = 10_000;

// ---------- REST ----------

async fn list_h(State(state): State<AppState>, ctx: Ctx, Query(p): Query<Page>) -> AppResult<Json<Value>> {
    let (rows, total) = store::list(&state.db, ctx.account_id, p.limit().min(100), p.offset()).await?;
    Ok(Json(json!({"data": rows, "total": total})))
}

async fn create_h(State(state): State<AppState>, ctx: Ctx, ApiJson(b): ApiJson<store::Input>) -> AppResult<(StatusCode, Json<Value>)> {
    let c = store::create(&state.db, ctx.account_id, b).await?;
    audit(&state, &ctx, "coupon.create", Some(&c.id.to_string()), json!({"name": c.name})).await;
    Ok((StatusCode::CREATED, Json(store::to_json(&state.db, &c).await?)))
}

async fn get_h(State(state): State<AppState>, ctx: Ctx, Path(id): Path<Uuid>) -> AppResult<Json<Value>> {
    let c = store::load(&state.db, ctx.account_id, id).await?;
    Ok(Json(store::to_json(&state.db, &c).await?))
}

async fn patch_h(State(state): State<AppState>, ctx: Ctx, Path(id): Path<Uuid>, ApiJson(b): ApiJson<store::Input>) -> AppResult<Json<Value>> {
    let c = store::update(&state.db, ctx.account_id, id, b).await?;
    audit(&state, &ctx, "coupon.update", Some(&c.id.to_string()), json!({"name": c.name})).await;
    Ok(Json(store::to_json(&state.db, &c).await?))
}

/// What deleting would do; refused while anything uses the coupon.
async fn delete_coupon(state: &AppState, ctx: &Ctx, id: Uuid, confirm: bool) -> AppResult<Value> {
    let c = store::load(&state.db, ctx.account_id, id).await?;
    let users = store::users(&state.db, &c).await?;
    if !users.is_empty() {
        return Err(AppError::Conflict(format!("coupon {:?} is used by {}; change those first", c.name, users.join(", "))));
    }
    if !confirm {
        let stock = store::stock(&state.db, &c).await?;
        return Ok(json!({"dry_run": true, "note": format!(
            "Nothing deleted. This permanently deletes coupon {:?} and its {} codes, including {} given to people and {} redeemed, and its report. Call again with confirm=true once the user approves.",
            c.name, stock["total"], stock["assigned"], stock["redeemed"]
        )}));
    }
    let codes = c.codes_total;
    store::delete(&state.db, &c).await?;
    audit(state, ctx, "coupon.delete", Some(&c.id.to_string()), json!({"name": c.name, "codes": codes})).await;
    Ok(json!({"deleted": true}))
}

#[derive(Deserialize)]
struct ConfirmQuery {
    confirm: Option<bool>,
}

async fn delete_h(State(state): State<AppState>, ctx: Ctx, Path(id): Path<Uuid>, Query(q): Query<ConfirmQuery>) -> AppResult<Response> {
    let v = delete_coupon(&state, &ctx, id, q.confirm == Some(true)).await?;
    if v["dry_run"] == true {
        return Err(AppError::bad(format!("{} (REST: DELETE /api/coupons/{id}?confirm=true)", v["note"].as_str().unwrap_or(""))));
    }
    Ok(StatusCode::NO_CONTENT.into_response())
}

async fn codes_h(State(state): State<AppState>, ctx: Ctx, Path(id): Path<Uuid>, Query(q): Query<store::CodeQuery>) -> AppResult<Json<Value>> {
    raw_codes_allowed(&ctx)?;
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

fn add_response(r: &store::UploadResult, coupon: Value) -> AppResult<Value> {
    if r.added == 0 && r.duplicates == 0 && r.refused == 0 {
        return Err(AppError::bad("no codes found: one code per line, 4 to 64 letters, digits, - or _"));
    }
    let mut v = serde_json::to_value(r)?;
    v["coupon"] = coupon;
    Ok(v)
}

async fn add_codes(state: &AppState, ctx: &Ctx, id: Uuid, b: AddBody) -> AppResult<Value> {
    let account_id = ctx.account_id;
    let c = store::load(&state.db, account_id, id).await?;
    let Some(lease) = store::Lease::take(&state.db, account_id).await? else { return Err(store::busy()) };
    let res = match (b.codes, b.text) {
        (Some(list), None) => store::add_list(&state.db, &c, &list).await,
        (None, Some(text)) => store::add_text(&state.db, &c, &text).await,
        _ => Err(AppError::bad("send either codes (a list) or text (pasted codes or a CSV)")),
    };
    lease.release(&state.db).await;
    let r = res?;
    audit_added(state, ctx, &c, &r).await;
    let c = store::load(&state.db, account_id, id).await?;
    add_response(&r, store::to_json(&state.db, &c).await?)
}

/// Counts only, never codes.
async fn audit_added(state: &AppState, ctx: &Ctx, c: &store::Coupon, r: &store::UploadResult) {
    audit(state, ctx, "coupon.codes_add", Some(&c.id.to_string()), json!({"name": c.name, "added": r.added, "duplicates": r.duplicates, "refused": r.refused})).await;
}

/// Add codes: JSON (`codes` or `text`, up to 2 MB), or the file itself (`text/csv` or `text/plain`,
/// up to 16 MB), read and inserted a batch at a time as it arrives.
async fn add_codes_h(State(state): State<AppState>, ctx: Ctx, Path(id): Path<Uuid>, headers: HeaderMap, body: Body) -> AppResult<Json<Value>> {
    let kind = headers.get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).unwrap_or("").to_ascii_lowercase();
    if kind.starts_with("application/json") {
        let bytes = axum::body::to_bytes(body, MAX_JSON_UPLOAD).await.map_err(|_| AppError::bad("JSON uploads are limited to 2 MB; send the file itself as text/csv"))?;
        let b: AddBody = serde_json::from_slice(&bytes).map_err(|e| AppError::bad(format!("invalid body: {e}")))?;
        return Ok(Json(add_codes(&state, &ctx, id, b).await?));
    }
    if !(kind.starts_with("text/csv") || kind.starts_with("text/plain")) {
        return Err(AppError::bad("send JSON, or the codes as text/csv or text/plain"));
    }
    let c = store::load(&state.db, ctx.account_id, id).await?;
    let Some(lease) = store::Lease::take(&state.db, ctx.account_id).await? else { return Err(store::busy()) };
    let res: AppResult<store::UploadResult> = async {
        let mut reader = store::LineReader::default();
        let mut stream = body.into_data_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|_| AppError::bad("the upload was interrupted"))?;
            reader.feed(&chunk)?;
            reader.flush(&state.db, &c, false).await?;
            if reader.stopped() {
                break;
            }
        }
        reader.finish()?;
        reader.flush(&state.db, &c, true).await?;
        Ok(reader.result)
    }
    .await;
    lease.release(&state.db).await;
    let r = res?;
    audit_added(&state, &ctx, &c, &r).await;
    let c = store::load(&state.db, ctx.account_id, id).await?;
    Ok(Json(add_response(&r, store::to_json(&state.db, &c).await?)?))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GenerateBody {
    count: usize,
    pattern: Option<store::Pattern>,
}

async fn generate(state: &AppState, ctx: &Ctx, id: Uuid, b: GenerateBody) -> AppResult<Value> {
    let account_id = ctx.account_id;
    let c = store::load(&state.db, account_id, id).await?;
    let (added, queued) = store::generate(&state.db, &c, b.count, b.pattern).await?;
    audit(state, ctx, "coupon.codes_generate", Some(&c.id.to_string()), json!({"name": c.name, "added": added, "queued": queued})).await;
    let c = store::load(&state.db, account_id, id).await?;
    Ok(json!({"added": added, "queued": queued, "coupon": store::to_json(&state.db, &c).await?}))
}

async fn generate_h(State(state): State<AppState>, ctx: Ctx, Path(id): Path<Uuid>, ApiJson(b): ApiJson<GenerateBody>) -> AppResult<Json<Value>> {
    Ok(Json(generate(&state, &ctx, id, b).await?))
}

#[derive(Deserialize)]
struct ExportQuery {
    status: Option<String>,
    format: Option<String>,
}

/// A coupon's codes, streamed a chunk at a time (a pool can hold 2 million): `format=csv` (default)
/// for spreadsheets, `format=codes` for one code per line exactly as stored.
async fn export_h(State(state): State<AppState>, ctx: Ctx, Path(id): Path<Uuid>, Query(q): Query<ExportQuery>) -> AppResult<Response> {
    raw_codes_allowed(&ctx)?;
    let c = store::load(&state.db, ctx.account_id, id).await?;
    let status = q.status.filter(|s| !s.is_empty() && s != "all");
    let format = store::Format::parse(q.format.as_deref())?;
    // Fail fast on a bad status rather than mid-stream.
    store::export_chunk(&state.db, &c, status.as_deref(), format, i64::MAX, false, 1).await?;
    audit(&state, &ctx, "coupon.export", Some(&c.id.to_string()), json!({"name": c.name, "status": status.as_deref().unwrap_or("all"), "format": q.format.as_deref().unwrap_or("csv")})).await;
    let (ext, mime) = if format == store::Format::Csv { ("csv", "text/csv; charset=utf-8") } else { ("txt", "text/plain; charset=utf-8") };
    let filename = format!("coupon-{}-{}.{ext}", c.name.replace(|ch: char| !ch.is_ascii_alphanumeric(), "-"), status.as_deref().unwrap_or("all"));
    let stream = futures_util::stream::unfold(Some((0i64, true)), move |cursor| {
        let (state, c, status) = (state.clone(), c.clone(), status.clone());
        async move {
            let (after, header) = cursor?;
            match store::export_chunk(&state.db, &c, status.as_deref(), format, after, header, 5000).await {
                Ok((text, last, n)) => {
                    if n == 0 && !header {
                        return None;
                    }
                    let next = if n == 0 { None } else { Some((last, false)) };
                    Some((Ok::<_, std::io::Error>(axum::body::Bytes::from(text)), next))
                }
                Err(e) => Some((Err(std::io::Error::other(e.public_message())), None)),
            }
        }
    });
    Ok((
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, mime.to_string()),
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

/// A campaign's coupon check before sending: problems (stop the send) and warnings (stock).
async fn campaign_check_h(State(state): State<AppState>, ctx: Ctx, Path(campaign_id): Path<Uuid>) -> AppResult<Json<Value>> {
    let c = crate::campaigns::load(&state.db, ctx.account_id, campaign_id).await?;
    let r = checks::campaign_check(&state, ctx.account_id, &c).await?;
    Ok(Json(json!({"problems": r.problems, "warnings": r.warnings})))
}

// ---------- MCP ----------

const COUPONS_DOC: &str = include_str!("../../../docs/COUPONS.md");

const TOOL_NAMES: &[&str] = &[
    "list_coupons",
    "get_coupon",
    "create_coupon",
    "update_coupon",
    "add_coupon_codes",
    "generate_coupon_codes",
    "export_coupon_codes",
    "delete_coupon",
    "coupon_report",
];

fn settings_schema() -> Value {
    json!({
        "description": {"type": "string", "maxLength": 500},
        "source": {"enum": ["uploaded", "generated"], "description": "uploaded: codes made in your store. generated: DumbMail makes them from pattern (export them and import into the store)."},
        "pattern": {"type": ["object", "null"], "properties": {
            "prefix": {"type": "string", "maxLength": 20, "description": "Letters, digits, - and _."},
            "length": {"type": "integer", "minimum": 6, "maximum": 24, "description": "Random characters after the prefix. Prefix and random part together are at least 8."},
            "alphabet": {"enum": ["letters_digits", "letters", "digits"], "description": "letters_digits (default) leaves out look-alikes 0, O, 1, I and L."}},
            "required": ["length"], "additionalProperties": false},
        "expires_at": {"type": ["string", "null"], "format": "date-time", "description": "Codes stop working on this date."},
        "expires_after_days": {"type": ["integer", "null"], "minimum": 1, "maximum": 3650, "description": "Or: each code expires this many days after it's first sent."},
        "new_code_each_time": {"type": "boolean", "description": "false (default): a person gets the same code in every message. true: a new code in every send (a retry of the same send keeps its code)."},
        "low_stock_threshold": {"type": ["integer", "null"], "minimum": 1, "maximum": 1000000, "description": "Alert when fewer codes than this are left."},
        "form_codes_per_hour": {"type": "integer", "minimum": 1, "maximum": 10000, "description": "Most codes sign-up forms may hand out per hour (default 200)."}
    })
}

fn tools() -> Vec<Value> {
    let id = json!({"type": "string", "description": "The coupon's id or name."});
    let status = json!({"enum": ["available", "assigned", "redeemed", "all"]});
    let mut create = settings_schema();
    create["name"] = json!({"type": "string", "maxLength": 64, "description": "What templates use: {% coupon_code 'NAME' %}. Letters, digits, spaces, . _ -. Can't change later."});
    let mut update = settings_schema();
    update["id"] = id.clone();
    vec![
        tool("list_coupons", "List coupons", "Coupons with their stock (total, available, given, redeemed), alerts (out of codes, low stock, campaigns and flows waiting for codes) and the template tags to use. No codes.",
            obj(json!({"limit": {"type": "integer", "minimum": 1, "maximum": 100}, "offset": {"type": "integer", "minimum": 0}}), &[]), true, false),
        tool("get_coupon", "Get a coupon", "One coupon's settings, stock and alerts, plus a page of its codes and who holds each (search by the start of a code, or by a person's email). Pass next_cursor as cursor for the next page. Shows raw codes: for people who manage coupons.",
            obj(json!({"id": id, "search": {"type": "string", "description": "Start of a code, or an email address."}, "status": status,
                       "limit": {"type": "integer", "minimum": 1, "maximum": 200}, "cursor": {"type": "integer", "description": "next_cursor from the previous page."}}), &["id"]), true, false),
        tool("create_coupon", "Create a coupon", "Create a pool of unique single-use codes. Then add codes with add_coupon_codes (made in your store) or generate_coupon_codes, and use {% coupon_code 'NAME' %} or a coupon block in the body of emails and texts, or set a sign-up form's config.coupon.",
            obj(create, &["name"]), false, false),
        tool("update_coupon", "Change a coupon", "Change a coupon's description, expiry, low-stock alert, per-send mode or form limit. The name can't change.", obj(update, &["id"]), false, false),
        tool("add_coupon_codes", "Add coupon codes", "Add codes made in your store: a list, or pasted text / CSV (one per line, first column, a header row is skipped). Codes are 4 to 64 letters, digits, - or _; other lines are refused and listed by line number. Up to 200,000 per call and 2,000,000 per coupon; one upload at a time per account. Codes already in the coupon (any letter case) are skipped. Adding codes lets campaigns and flows waiting for this coupon carry on.",
            obj(json!({"id": id, "codes": {"type": "array", "items": {"type": "string", "maxLength": 64}, "maxItems": 200000}, "text": {"type": "string"}}), &["id"]), false, false),
        tool("generate_coupon_codes", "Generate coupon codes", "Make new random codes from the coupon's pattern (or the one given, which is saved). Up to 5,000 are made at once; more are made in the background (see stock.generating). The store must accept them: export them (export_coupon_codes, format codes) and import them into the store before sending.",
            obj(json!({"id": id, "count": {"type": "integer", "minimum": 1, "maximum": 200000}, "pattern": settings_schema()["pattern"]}), &["id", "count"]), false, false),
        tool("export_coupon_codes", "Export coupon codes", "A page of a coupon's codes as text: format codes (one per line, exactly as stored, uploads back unchanged) or csv (with status, holder's email and dates, safe for spreadsheets). Up to 10,000 per call; pass next_after as after for the next page. Shows raw codes: for people who manage coupons.",
            obj(json!({"id": id, "status": status, "format": {"enum": ["codes", "csv"]}, "after": {"type": "integer", "minimum": 0}, "limit": {"type": "integer", "minimum": 1, "maximum": MCP_EXPORT_MAX}}), &["id"]), true, false),
        tool("delete_coupon", "Delete a coupon", "Delete a coupon and all its codes, including who got which code and redemptions. Refused while a segment, flow, unfinished campaign or form uses it (the error says which). Without confirm=true it only describes what would happen; get the user's approval first.",
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
            raw_codes_allowed(ctx)?;
            let c = store::find(&state.db, acct, &key()?).await?;
            let q = store::CodeQuery { q: a["search"].as_str().map(str::to_string), status: a["status"].as_str().map(str::to_string), limit: a["limit"].as_i64(), cursor: a["cursor"].as_i64() };
            let mut v = store::to_json(&state.db, &c).await?;
            v["codes"] = store::codes(&state.db, &c, &q).await?;
            Ok(v)
        }
        "create_coupon" => {
            let c = store::create(&state.db, acct, from(a.clone())?).await?;
            audit(state, ctx, "coupon.create", Some(&c.id.to_string()), json!({"name": c.name})).await;
            store::to_json(&state.db, &c).await
        }
        "update_coupon" => {
            let c = store::find(&state.db, acct, &key()?).await?;
            let mut body = a.clone();
            if let Some(o) = body.as_object_mut() {
                o.remove("id");
            }
            let c = store::update(&state.db, acct, c.id, from(body)?).await?;
            audit(state, ctx, "coupon.update", Some(&c.id.to_string()), json!({"name": c.name})).await;
            store::to_json(&state.db, &c).await
        }
        "add_coupon_codes" => {
            let c = store::find(&state.db, acct, &key()?).await?;
            let body = AddBody { codes: from(a["codes"].clone())?, text: a["text"].as_str().map(str::to_string) };
            add_codes(state, ctx, c.id, body).await
        }
        "generate_coupon_codes" => {
            let c = store::find(&state.db, acct, &key()?).await?;
            let count = a["count"].as_u64().ok_or_else(|| AppError::bad("count is required"))? as usize;
            let pattern = if a["pattern"].is_null() { None } else { Some(from(a["pattern"].clone())?) };
            generate(state, ctx, c.id, GenerateBody { count, pattern }).await
        }
        "export_coupon_codes" => {
            raw_codes_allowed(ctx)?;
            let c = store::find(&state.db, acct, &key()?).await?;
            let format = store::Format::parse(a["format"].as_str())?;
            let after = a["after"].as_i64().unwrap_or(0).max(0);
            let limit = a["limit"].as_i64().unwrap_or(1000).clamp(1, MCP_EXPORT_MAX);
            let (text, last, n) = store::export_chunk(&state.db, &c, a["status"].as_str(), format, after, after == 0, limit).await?;
            audit(state, ctx, "coupon.export", Some(&c.id.to_string()), json!({"name": c.name, "codes": n, "after": after})).await;
            Ok(json!({"text": text, "count": n, "next_after": if n as i64 == limit { Some(last) } else { None }}))
        }
        "delete_coupon" => {
            let c = store::find(&state.db, acct, &key()?).await?;
            delete_coupon(state, ctx, c.id, a["confirm"].as_bool() == Some(true)).await
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
            .route("/api/coupons/campaign-check/{campaign_id}", get(campaign_check_h))
            .route("/api/coupons/{id}", get(get_h).patch(patch_h).delete(delete_h))
            .route("/api/coupons/{id}/codes", get(codes_h).post(add_codes_h))
            .route("/api/coupons/{id}/generate", post(generate_h))
            .route("/api/coupons/{id}/export", get(export_h))
            .route("/api/coupons/{id}/report", get(report_h))
    }

    fn access(&self) -> Access {
        Access { routes: ROUTES, tools: TOOLS }
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
            description: "Coupons (pools of single-use codes), the {% coupon_code %} tag and coupon block, sign-up form codes, redemptions, limits and permissions.",
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
            "coupons_generate" => Some(match id() {
                Ok(i) => store::generate_job(&state.db, i).await,
                Err(e) => Err(e),
            }),
            _ => None,
        }
    }

    async fn tick(&self, state: &AppState, tick: u64) -> anyhow::Result<()> {
        if tick.is_multiple_of(3) {
            redeem::reconcile(&state.db).await?;
            assign::check_low_stock(&state.db).await?;
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
