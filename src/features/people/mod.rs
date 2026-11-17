//! Profile tools: consent history, the suppression list, merging duplicates, bulk actions, privacy
//! requests (export or delete one person), "do not sell or share", and internal notes.
//!
//! Other features record consent changes with [`consent::record_consent_change`] (or the setters
//! in [`consent`]), and read `profiles.ads_opt_out` for "do not sell or share". Tables keyed by a
//! profile id belong in [`merge::PROFILE_TABLES`] so merges and deletions cover them.

pub mod bulk;
pub mod consent;
pub mod merge;
pub mod privacy;
pub mod suppression;

use async_trait::async_trait;
use axum::extract::{Path, Query, State};
use axum::http::header;
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, patch, post};
use axum::{Json, Router};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::{json, Value};
use sqlx::PgPool;
use uuid::Uuid;

pub use consent::{record_consent_change, with_source, Change, Channel, ConsentSource};

use super::{Feature, McpDoc};
use crate::api::ApiJson;
use crate::auth::Ctx;
use crate::error::{AppError, AppResult};
use crate::mcp::{from, obj, tool};
use crate::models::{Profile, PROFILE_COLS};
use crate::state::AppState;

pub struct Module;

const NOTE_MAX: usize = 5_000;

pub async fn load_profile(db: &PgPool, account_id: Uuid, id: Uuid) -> AppResult<Profile> {
    sqlx::query_as::<_, Profile>(&format!("SELECT {PROFILE_COLS} FROM profiles p WHERE p.id = $1 AND p.account_id = $2"))
        .bind(id)
        .bind(account_id)
        .fetch_optional(db)
        .await?
        .ok_or_else(|| AppError::not_found("profile"))
}

// ---------- Profile page ----------

/// Everything the profile page shows from this feature.
pub async fn profile_summary(db: &PgPool, account_id: Uuid, id: Uuid) -> AppResult<Value> {
    load_profile(db, account_id, id).await?;
    let (history, total) = consent::history(db, account_id, id, 50, 0).await?;
    let ads: (bool, Option<DateTime<Utc>>, Option<String>) = sqlx::query_as("SELECT ads_opt_out, ads_opt_out_at, ads_opt_out_source FROM profiles WHERE id = $1").bind(id).fetch_one(db).await?;
    Ok(json!({
        "reachability": consent::reachability(db, account_id, id).await?,
        "ads_opt_out": ads.0, "ads_opt_out_at": ads.1, "ads_opt_out_source": ads.2,
        "consent_history": history, "consent_history_total": total,
        "history_started_at": consent::history_started_at(db, account_id).await?,
        "notes": notes(db, account_id, id).await?,
    }))
}

pub async fn notes(db: &PgPool, account_id: Uuid, profile_id: Uuid) -> AppResult<Vec<Value>> {
    let rows: Vec<(Uuid, Option<Uuid>, String, String, DateTime<Utc>)> = sqlx::query_as(
        "SELECT id, author_user_id, author_label, body, created_at FROM profile_notes WHERE account_id = $1 AND profile_id = $2 ORDER BY created_at DESC LIMIT 500",
    )
    .bind(account_id)
    .bind(profile_id)
    .fetch_all(db)
    .await?;
    Ok(rows.into_iter().map(|r| json!({"id": r.0, "author_user_id": r.1, "author": r.2, "body": r.3, "created_at": r.4})).collect())
}

async fn author_label(db: &PgPool, src: &ConsentSource) -> AppResult<String> {
    if let Some(u) = src.actor_user_id {
        let email: Option<String> = sqlx::query_scalar("SELECT email FROM users WHERE id = $1").bind(u).fetch_optional(db).await?;
        if let Some(e) = email {
            return Ok(e);
        }
    }
    Ok(match src.via {
        "mcp" => "AI assistant (MCP)".into(),
        _ => "API key".into(),
    })
}

pub async fn add_note(db: &PgPool, account_id: Uuid, profile_id: Uuid, body: &str, src: &ConsentSource) -> AppResult<Value> {
    load_profile(db, account_id, profile_id).await?;
    let body = body.trim();
    if body.is_empty() {
        return Err(AppError::bad("write a note first"));
    }
    if body.chars().count() > NOTE_MAX {
        return Err(AppError::bad(format!("notes can be at most {NOTE_MAX} characters")));
    }
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM profile_notes WHERE profile_id = $1").bind(profile_id).fetch_one(db).await?;
    if count >= 500 {
        return Err(AppError::bad("this profile has 500 notes; delete some first"));
    }
    let id = Uuid::now_v7();
    let label = author_label(db, src).await?;
    let at: DateTime<Utc> = sqlx::query_scalar("INSERT INTO profile_notes (id, account_id, profile_id, author_user_id, author_label, body) VALUES ($1, $2, $3, $4, $5, $6) RETURNING created_at")
        .bind(id)
        .bind(account_id)
        .bind(profile_id)
        .bind(src.actor_user_id)
        .bind(&label)
        .bind(body)
        .fetch_one(db)
        .await?;
    Ok(json!({"id": id, "author_user_id": src.actor_user_id, "author": label, "body": body, "created_at": at}))
}

pub async fn delete_note(db: &PgPool, account_id: Uuid, profile_id: Uuid, note_id: Uuid) -> AppResult<()> {
    let n = sqlx::query("DELETE FROM profile_notes WHERE id = $1 AND profile_id = $2 AND account_id = $3").bind(note_id).bind(profile_id).bind(account_id).execute(db).await?.rows_affected();
    if n == 0 {
        return Err(AppError::not_found("note"));
    }
    Ok(())
}

// ---------- HTTP ----------

#[derive(Deserialize)]
struct PageQuery {
    limit: Option<i64>,
    offset: Option<i64>,
}

async fn summary_h(State(s): State<AppState>, ctx: Ctx, Path(id): Path<Uuid>) -> AppResult<Json<Value>> {
    Ok(Json(profile_summary(&s.db, ctx.account_id, id).await?))
}

async fn history_h(State(s): State<AppState>, ctx: Ctx, Path(id): Path<Uuid>, Query(q): Query<PageQuery>) -> AppResult<Json<Value>> {
    load_profile(&s.db, ctx.account_id, id).await?;
    let (data, total) = consent::history(&s.db, ctx.account_id, id, q.limit.unwrap_or(50), q.offset.unwrap_or(0)).await?;
    Ok(Json(json!({"data": data, "total": total, "history_started_at": consent::history_started_at(&s.db, ctx.account_id).await?})))
}

#[derive(Deserialize)]
struct PrivacyBody {
    ads_opt_out: bool,
}

async fn privacy_h(State(s): State<AppState>, ctx: Ctx, Path(id): Path<Uuid>, ApiJson(b): ApiJson<PrivacyBody>) -> AppResult<Json<Value>> {
    let changed = consent::set_ads_opt_out(&s.db, ctx.account_id, id, b.ads_opt_out, &ConsentSource::from_ctx(&ctx)).await?;
    Ok(Json(json!({"ads_opt_out": b.ads_opt_out, "changed": changed})))
}

async fn notes_h(State(s): State<AppState>, ctx: Ctx, Path(id): Path<Uuid>) -> AppResult<Json<Value>> {
    load_profile(&s.db, ctx.account_id, id).await?;
    Ok(Json(json!({"data": notes(&s.db, ctx.account_id, id).await?})))
}

#[derive(Deserialize)]
struct NoteBody {
    body: String,
}

async fn add_note_h(State(s): State<AppState>, ctx: Ctx, Path(id): Path<Uuid>, ApiJson(b): ApiJson<NoteBody>) -> AppResult<Json<Value>> {
    Ok(Json(add_note(&s.db, ctx.account_id, id, &b.body, &ConsentSource::from_ctx(&ctx)).await?))
}

async fn delete_note_h(State(s): State<AppState>, ctx: Ctx, Path((id, note)): Path<(Uuid, Uuid)>) -> AppResult<Json<Value>> {
    delete_note(&s.db, ctx.account_id, id, note).await?;
    Ok(Json(json!({"ok": true})))
}

async fn suppressions_h(State(s): State<AppState>, ctx: Ctx, Query(q): Query<suppression::SuppressionQuery>) -> AppResult<Json<Value>> {
    Ok(Json(suppression::list(&s.db, ctx.account_id, &q).await?))
}

fn download(filename: &str, content_type: &str, bytes: Vec<u8>) -> Response {
    let safe: String = filename.chars().filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.')).collect();
    (
        [
            (header::CONTENT_TYPE, content_type.to_string()),
            (header::CONTENT_DISPOSITION, format!("attachment; filename=\"{safe}\"")),
            (header::CACHE_CONTROL, "no-store".to_string()),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff".to_string()),
        ],
        bytes,
    )
        .into_response()
}

async fn suppressions_export_h(State(s): State<AppState>, ctx: Ctx, Query(q): Query<suppression::SuppressionQuery>) -> AppResult<Response> {
    let csv = suppression::export_csv(&s.db, ctx.account_id, &q).await?;
    Ok(download(&format!("suppressions-{}.csv", Utc::now().format("%Y-%m-%d")), "text/csv; charset=utf-8", csv.into_bytes()))
}

async fn suppress_h(State(s): State<AppState>, ctx: Ctx, ApiJson(b): ApiJson<suppression::SuppressBody>) -> AppResult<Json<Value>> {
    let src = ConsentSource::from_ctx(&ctx).method("suppression_list");
    Ok(Json(suppression::suppress_many(&s, ctx.account_id, &b, &src).await?))
}

#[derive(Deserialize)]
pub struct UnsuppressBody {
    pub profile_id: Uuid,
    pub channel: String,
    #[serde(default)]
    pub confirm: bool,
}

pub async fn unsuppress(db: &PgPool, account_id: Uuid, b: &UnsuppressBody, src: &ConsentSource) -> AppResult<Value> {
    let ch = Channel::parse(&b.channel)?;
    let mut conn = db.acquire().await?;
    let st = consent::load_state(&mut conn, account_id, b.profile_id, false).await?.ok_or_else(|| AppError::not_found("profile"))?;
    drop(conn);
    let (reason, consent_now) = match ch {
        Channel::Email => (st.email_suppressed_reason.as_deref(), st.email_consent.as_str()),
        Channel::Sms => (st.sms_suppressed_reason.as_deref(), st.sms_consent.as_str()),
    };
    if let Some(why) = consent::why_not_unsuppressible(ch, reason, consent_now) {
        return Err(AppError::Conflict(why));
    }
    if !b.confirm {
        return Ok(json!({"dry_run": true, "note": "Nothing changed yet. This lifts the manual suppression; their consent stays as it is (it never subscribes them). Call again with confirm=true once the user approves."}));
    }
    consent::unsuppress_manual(db, account_id, b.profile_id, ch, &src.clone().method("suppression_list")).await?;
    Ok(json!({"ok": true, "profile_id": b.profile_id, "channel": ch.as_str()}))
}

async fn unsuppress_h(State(s): State<AppState>, ctx: Ctx, ApiJson(b): ApiJson<UnsuppressBody>) -> AppResult<Json<Value>> {
    Ok(Json(unsuppress(&s.db, ctx.account_id, &b, &ConsentSource::from_ctx(&ctx)).await?))
}

#[derive(Deserialize)]
struct LimitQuery {
    limit: Option<i64>,
}

async fn duplicates_h(State(s): State<AppState>, ctx: Ctx, Query(q): Query<LimitQuery>) -> AppResult<Json<Value>> {
    Ok(Json(json!({"data": merge::find_duplicates(&s.db, ctx.account_id, q.limit.unwrap_or(50)).await?})))
}

async fn merge_h(State(s): State<AppState>, ctx: Ctx, ApiJson(b): ApiJson<merge::MergeBody>) -> AppResult<Json<Value>> {
    Ok(Json(merge::merge(&s.db, ctx.account_id, &b, &ConsentSource::from_ctx(&ctx)).await?))
}

async fn bulk_start_h(State(s): State<AppState>, ctx: Ctx, ApiJson(b): ApiJson<bulk::BulkBody>) -> AppResult<Json<Value>> {
    Ok(Json(bulk::start(&s, ctx.account_id, &b, &ConsentSource::from_ctx(&ctx)).await?))
}

async fn bulk_list_h(State(s): State<AppState>, ctx: Ctx, Query(q): Query<LimitQuery>) -> AppResult<Json<Value>> {
    Ok(Json(json!({"data": bulk::recent(&s.db, ctx.account_id, q.limit.unwrap_or(20)).await?})))
}

async fn bulk_get_h(State(s): State<AppState>, ctx: Ctx, Path(id): Path<Uuid>) -> AppResult<Json<Value>> {
    Ok(Json(bulk::get(&s.db, ctx.account_id, id).await?))
}

async fn export_person_h(State(s): State<AppState>, ctx: Ctx, ApiJson(b): ApiJson<privacy::PersonQuery>) -> AppResult<Json<Value>> {
    let mut out = privacy::export_person(&s, ctx.account_id, &b, &ConsentSource::from_ctx(&ctx).method("privacy_request")).await?;
    // The dashboard downloads the files; it doesn't need the data twice.
    if let Some(o) = out.as_object_mut() {
        o.remove("data");
    }
    Ok(Json(out))
}

async fn delete_person_h(State(s): State<AppState>, ctx: Ctx, ApiJson(b): ApiJson<privacy::PersonQuery>) -> AppResult<Json<Value>> {
    Ok(Json(privacy::delete_person(&s, ctx.account_id, &b, &ConsentSource::from_ctx(&ctx).method("privacy_request")).await?))
}

async fn requests_h(State(s): State<AppState>, ctx: Ctx, Query(q): Query<PageQuery>) -> AppResult<Json<Value>> {
    let (data, total) = privacy::requests(&s.db, ctx.account_id, q.limit.unwrap_or(50), q.offset.unwrap_or(0)).await?;
    Ok(Json(json!({"data": data, "total": total})))
}

async fn file_h(State(s): State<AppState>, ctx: Ctx, Path(id): Path<Uuid>) -> AppResult<Response> {
    let (name, ct, bytes) = privacy::file(&s.db, ctx.account_id, id).await?;
    Ok(download(&name, &ct, bytes))
}

// ---------- MCP ----------

async fn resolve_profile(db: &PgPool, account_id: Uuid, a: &Value) -> AppResult<Uuid> {
    if let Some(id) = a["profile_id"].as_str().filter(|s| !s.is_empty()) {
        return id.parse().map_err(|_| AppError::bad("`profile_id` must be an id"));
    }
    let input = crate::audience::ProfileInput { email: a["email"].as_str().map(String::from), phone_number: a["phone_number"].as_str().map(String::from), ..Default::default() };
    if input.email.is_none() && input.phone_number.is_none() {
        return Err(AppError::bad("give profile_id, email or phone_number"));
    }
    Ok(crate::audience::find_profile(db, account_id, &input).await?.ok_or_else(|| AppError::not_found("profile"))?.id)
}

fn uuid_arg(a: &Value, k: &str) -> AppResult<Uuid> {
    a[k].as_str().ok_or_else(|| AppError::bad(format!("`{k}` is required")))?.parse().map_err(|_| AppError::bad(format!("`{k}` must be an id")))
}

async fn mcp(state: &AppState, ctx: &Ctx, name: &str, a: &Value) -> AppResult<Value> {
    let acct = ctx.account_id;
    let db = &state.db;
    let src = ConsentSource::mcp(ctx);
    match name {
        "get_consent_history" => {
            let id = resolve_profile(db, acct, a).await?;
            let (history, total) = consent::history(db, acct, id, a["limit"].as_i64().unwrap_or(50), a["offset"].as_i64().unwrap_or(0)).await?;
            let ads: bool = sqlx::query_scalar("SELECT ads_opt_out FROM profiles WHERE id = $1").bind(id).fetch_one(db).await?;
            Ok(json!({
                "profile_id": id, "history": history, "total": total,
                "history_started_at": consent::history_started_at(db, acct).await?,
                "reachability": consent::reachability(db, acct, id).await?,
                "do_not_sell_or_share": ads,
            }))
        }
        "list_suppressions" => {
            let q = suppression::SuppressionQuery {
                q: a["query"].as_str().map(String::from),
                channel: a["channel"].as_str().map(String::from),
                reason: a["reason"].as_str().map(String::from),
                from: a["from"].as_str().map(String::from),
                to: a["to"].as_str().map(String::from),
                limit: a["limit"].as_i64(),
                offset: a["offset"].as_i64(),
            };
            suppression::list(db, acct, &q).await
        }
        "suppress_profiles" => suppression::suppress_many(state, acct, &from(a.clone())?, &src.method("suppression_list")).await,
        "unsuppress_profile" => unsuppress(db, acct, &from(a.clone())?, &src).await,
        "find_duplicate_profiles" => Ok(json!({"groups": merge::find_duplicates(db, acct, a["limit"].as_i64().unwrap_or(50)).await?})),
        "merge_profiles" => merge::merge(db, acct, &from(a.clone())?, &src).await,
        "start_bulk_action" => bulk::start(state, acct, &from(a.clone())?, &src).await,
        "get_bulk_action" => match a["id"].as_str().filter(|s| !s.is_empty()) {
            Some(_) => bulk::get(db, acct, uuid_arg(a, "id")?).await,
            None => Ok(json!({"bulk_actions": bulk::recent(db, acct, 10).await?})),
        },
        "export_person_data" => privacy::export_person(state, acct, &from(a.clone())?, &src.method("privacy_request")).await,
        "delete_person_data" => privacy::delete_person(state, acct, &from(a.clone())?, &src.method("privacy_request")).await,
        "list_privacy_requests" => {
            let (data, total) = privacy::requests(db, acct, a["limit"].as_i64().unwrap_or(50), a["offset"].as_i64().unwrap_or(0)).await?;
            Ok(json!({"requests": data, "total": total}))
        }
        "set_do_not_sell" => {
            let id = resolve_profile(db, acct, a).await?;
            let v = a["do_not_sell"].as_bool().ok_or_else(|| AppError::bad("`do_not_sell` (true or false) is required"))?;
            let changed = consent::set_ads_opt_out(db, acct, id, v, &src).await?;
            Ok(json!({"profile_id": id, "do_not_sell_or_share": v, "changed": changed}))
        }
        "list_profile_notes" => {
            let id = resolve_profile(db, acct, a).await?;
            Ok(json!({"profile_id": id, "notes": notes(db, acct, id).await?}))
        }
        "add_profile_note" => {
            let id = resolve_profile(db, acct, a).await?;
            add_note(db, acct, id, a["note"].as_str().unwrap_or(""), &src).await
        }
        "delete_profile_note" => {
            let id = resolve_profile(db, acct, a).await?;
            let note = uuid_arg(a, "note_id")?;
            if a["confirm"].as_bool() != Some(true) {
                return Ok(json!({"dry_run": true, "note": "Nothing deleted. Call again with confirm=true once the user approves deleting this note."}));
            }
            delete_note(db, acct, id, note).await?;
            Ok(json!({"ok": true}))
        }
        _ => Err(AppError::bad("unknown tool")),
    }
}

const TOOLS: &[&str] = &[
    "get_consent_history", "list_suppressions", "suppress_profiles", "unsuppress_profile", "find_duplicate_profiles", "merge_profiles", "start_bulk_action",
    "get_bulk_action", "export_person_data", "delete_person_data", "list_privacy_requests", "set_do_not_sell", "list_profile_notes", "add_profile_note", "delete_profile_note",
];

fn mcp_tools() -> Vec<Value> {
    let who = json!({"profile_id": {"type": "string"}, "email": {"type": "string"}, "phone_number": {"type": "string"}});
    let with = |extra: Value| {
        let mut o = who.clone();
        if let (Some(m), Value::Object(e)) = (o.as_object_mut(), extra) {
            m.extend(e);
        }
        o
    };
    let selection = json!({"type": "object", "description": "Which profiles: profile_ids (up to 10,000), or everyone matching q (search), list_id and/or segment_id.", "properties": {
        "profile_ids": {"type": "array", "items": {"type": "string"}, "maxItems": 10_000}, "q": {"type": "string"}, "list_id": {"type": "string"}, "segment_id": {"type": "string"}}, "additionalProperties": false});
    vec![
        tool("get_consent_history", "Get a profile's consent history", "Every recorded change to the person's email and SMS consent, suppression and \"do not sell or share\" choice, newest first, with how it happened (form, import, unsubscribe link, STOP reply, bounce…), when, and the IP address or browser where known. Also says whether they can be emailed or texted right now and why not. History starts at history_started_at; earlier changes weren't recorded.",
            obj(with(json!({"limit": {"type": "integer", "minimum": 1, "maximum": 500}, "offset": {"type": "integer", "minimum": 0}})), &[]), true, false),
        tool("list_suppressions", "List suppressed people", "Everyone who can't be emailed or texted and why: unsubscribed, hard bounce, spam complaint, suppressed by hand, or (email) waiting to confirm a double opt-in sign-up. Filter by channel, reason, search text and date range (YYYY-MM-DD in the account's time zone).",
            obj(json!({"query": {"type": "string"}, "channel": {"enum": ["email", "sms"]}, "reason": {"enum": suppression::REASONS}, "from": {"type": "string", "format": "date"}, "to": {"type": "string", "format": "date"}, "limit": {"type": "integer", "minimum": 1, "maximum": 500}, "offset": {"type": "integer", "minimum": 0}}), &[]), true, false),
        tool("suppress_profiles", "Suppress addresses", "Add email addresses or phone numbers to the suppression list (profiles are created if needed). Suppressed people get no marketing on that channel. Always allowed. Without confirm=true it returns a dry run with the count.",
            obj(json!({"addresses": {"type": "array", "items": {"type": "string"}, "maxItems": suppression::REQUEST_MAX}, "confirm": {"type": "boolean"}}), &["addresses"]), false, true),
        tool("unsuppress_profile", "Lift a manual suppression", "Lift a suppression that was added by hand. Bounces, spam complaints and a person's own unsubscribe can't be lifted (the error says why). Consent stays as it is: this never subscribes anyone. Requires confirm=true.",
            obj(json!({"profile_id": {"type": "string"}, "channel": {"enum": ["email", "sms"]}, "confirm": {"type": "boolean"}}), &["profile_id", "channel"]), false, true),
        tool("find_duplicate_profiles", "Find possible duplicate profiles", "Groups of profiles that look like one person: the same Gmail inbox spelled differently, the same email in another case, the same phone number written differently, the same external id, or identifiers that a sign-up or API call linked but that sit on two profiles. Only suggests; use merge_profiles to merge.",
            obj(json!({"limit": {"type": "integer", "minimum": 1, "maximum": 200}}), &[]), true, false),
        tool("merge_profiles", "Merge two profiles", "Merge merge_id into keep_id: events, messages, lists, flow runs, sign-ups, notes and consent history move to the kept profile, then the other is removed. The kept profile's values win unless choices says {\"field\": \"merge\"} (fields: email, phone_number, external_id, first_name, last_name, properties.<key>). Consent becomes the more restrictive of the two on each channel and suppressions carry over. Without confirm=true it returns the merged result without changing anything.",
            obj(json!({"keep_id": {"type": "string"}, "merge_id": {"type": "string"}, "choices": {"type": "object", "additionalProperties": {"enum": ["keep", "merge"]}}, "confirm": {"type": "boolean"}}), &["keep_id", "merge_id"]), false, true),
        tool("start_bulk_action", "Start a bulk action on profiles", "Act on many profiles in the background: add_to_list or remove_from_list (list_id), set_property (key, value) or clear_property (key), suppress (channels: email and/or sms), delete, or export (CSV download). There's no bulk subscribe. First call without confirm to get the count; then call with confirm=true and expected_count set to that count (if the matching set changed, nothing runs). One bulk action per account at a time; check progress with get_bulk_action.",
            obj(json!({"action": {"enum": bulk::ACTIONS}, "selection": selection, "list_id": {"type": "string"}, "key": {"type": "string"}, "value": {}, "channels": {"type": "array", "items": {"enum": ["email", "sms"]}}, "confirm": {"type": "boolean"}, "expected_count": {"type": "integer", "minimum": 0}}), &["action", "selection"]), false, true),
        tool("get_bulk_action", "Get a bulk action's progress", "Status, progress and result of a bulk action (and the download link for an export). Without id, the 10 most recent.",
            obj(json!({"id": {"type": "string"}}), &[]), true, false),
        tool("export_person_data", "Export one person's data", "For a privacy (access) request: everything held about the person found by email or phone_number (profile, consent history, events, messages with subjects and outcomes, lists, segments, form sign-ups, internal notes). Returns the data and two download links (JSON and a readable HTML page) that need sign-in or an API key and expire in 7 days. The request is logged.",
            obj(json!({"email": {"type": "string"}, "phone_number": {"type": "string"}}), &[]), false, false),
        tool("delete_person_data", "Delete one person's data", "For a privacy (erasure) request: delete the person found by email or phone_number with all their events, messages, list memberships and consent history. If they had unsubscribed or were suppressed, only a one-way code of the address is kept so they stay suppressed if re-imported. Can't be undone; without confirm=true it shows what would be deleted. The request is logged.",
            obj(json!({"email": {"type": "string"}, "phone_number": {"type": "string"}, "confirm": {"type": "boolean"}}), &[]), false, true),
        tool("list_privacy_requests", "List privacy requests", "The log of export and delete requests: when, who made them, and what was done (the person's data isn't kept).",
            obj(json!({"limit": {"type": "integer", "minimum": 1, "maximum": 200}, "offset": {"type": "integer", "minimum": 0}}), &[]), true, false),
        tool("set_do_not_sell", "Set \"do not sell or share\"", "Turn a person's \"do not sell or share my personal information\" flag on or off (other features, like ad audience syncs, leave flagged people out). The website snippet also turns it on when a visitor's browser sends Global Privacy Control.",
            obj(with(json!({"do_not_sell": {"type": "boolean"}})), &["do_not_sell"]), false, false),
        tool("list_profile_notes", "List a profile's notes", "Internal notes your team wrote on a profile, newest first, with who wrote each and when.", obj(who.clone(), &[]), true, false),
        tool("add_profile_note", "Add a note to a profile", "Add an internal note (never shown to the person). Up to 5,000 characters.", obj(with(json!({"note": {"type": "string", "maxLength": NOTE_MAX}})), &["note"]), false, false),
        tool("delete_profile_note", "Delete a profile note", "Delete one internal note. Requires confirm=true.", obj(with(json!({"note_id": {"type": "string"}, "confirm": {"type": "boolean"}})), &["note_id"]), false, true),
    ]
}

#[async_trait]
impl Feature for Module {
    fn name(&self) -> &'static str {
        "people"
    }

    fn routes(&self) -> Router<AppState> {
        Router::new()
            .route("/api/people/profiles/{id}", get(summary_h))
            .route("/api/people/profiles/{id}/consent", get(history_h))
            .route("/api/people/profiles/{id}/privacy", patch(privacy_h))
            .route("/api/people/profiles/{id}/notes", get(notes_h).post(add_note_h))
            .route("/api/people/profiles/{id}/notes/{note_id}", delete(delete_note_h))
            .route("/api/people/suppressions", get(suppressions_h).post(suppress_h))
            .route("/api/people/suppressions/export", get(suppressions_export_h))
            .route("/api/people/suppressions/unsuppress", post(unsuppress_h))
            .route("/api/people/duplicates", get(duplicates_h))
            .route("/api/people/merge", post(merge_h))
            .route("/api/people/bulk", get(bulk_list_h).post(bulk_start_h))
            .route("/api/people/bulk/{id}", get(bulk_get_h))
            .route("/api/people/privacy/export", post(export_person_h))
            .route("/api/people/privacy/delete", post(delete_person_h))
            .route("/api/people/privacy/requests", get(requests_h))
            .route("/api/people/files/{id}", get(file_h))
    }

    fn mcp_tools(&self) -> Vec<Value> {
        mcp_tools()
    }

    async fn mcp_call(&self, state: &AppState, ctx: &Ctx, name: &str, args: &Value) -> Option<AppResult<Value>> {
        if !TOOLS.contains(&name) {
            return None;
        }
        Some(with_source(ConsentSource::mcp(ctx), mcp(state, ctx, name, args)).await)
    }

    fn mcp_docs(&self) -> Vec<McpDoc> {
        vec![McpDoc {
            slug: "people",
            name: "Profile tools and privacy",
            description: "Consent history, the suppression list, merging duplicates, bulk actions, privacy requests and \"do not sell or share\"",
            text: include_str!("../../../docs/PEOPLE.md"),
        }]
    }

    async fn run_job(&self, state: &AppState, kind: &str, payload: &Value) -> Option<anyhow::Result<()>> {
        (kind == bulk::JOB).then_some(())?;
        Some(bulk::run(state, payload).await)
    }

    async fn tick(&self, state: &AppState, tick: u64) -> anyhow::Result<()> {
        if tick.is_multiple_of(15) {
            bulk::fail_stuck(&state.db).await?;
        }
        if tick.is_multiple_of(180) {
            sqlx::query("DELETE FROM people_files WHERE expires_at < now()").execute(&state.db).await?;
        }
        Ok(())
    }
}
