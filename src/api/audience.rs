use axum::extract::{Path, Query, State};
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::{json, Value};
use sqlx::QueryBuilder;
use uuid::Uuid;

use super::{page, ApiJson, Page};
use crate::audience::{self as core, NewEvent, ProfileInput};
use crate::auth::Ctx;
use crate::error::{AppError, AppResult};
use crate::models::{EventRow, List, Profile, Segment, PROFILE_COLS};
use crate::segments;
use crate::state::AppState;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/profiles", get(list_profiles_h).post(create_profile_h))
        .route("/api/profiles/import", post(import_h))
        .route("/api/profiles/{id}", get(get_profile_h).patch(patch_profile_h).delete(delete_profile_h))
        .route("/api/lists", get(lists_h).post(create_list_h))
        .route("/api/lists/{id}", get(get_list_h).patch(patch_list_h).delete(delete_list_h))
        .route("/api/lists/{id}/members", post(add_members_h).delete(remove_members_h))
        .route("/api/segments", get(segments_h).post(create_segment_h))
        .route("/api/segments/preview", post(preview_h))
        .route("/api/segments/{id}", get(get_segment_h).patch(patch_segment_h).delete(delete_segment_h))
        .route("/api/events", get(events_h).post(track_h))
        .route("/api/metrics", get(metrics_h))
        .route("/api/metrics/query", post(query_metric_h))
}

// ---------- Profiles ----------

#[derive(Deserialize, Default)]
pub struct ProfileQuery {
    pub q: Option<String>,
    pub list_id: Option<Uuid>,
    pub segment_id: Option<Uuid>,
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

pub async fn list_profiles(state: &AppState, account_id: Uuid, q: &ProfileQuery) -> AppResult<(Vec<Profile>, i64)> {
    let pg = Page { limit: q.limit, offset: q.offset };
    let seg_def = match q.segment_id {
        Some(id) => {
            let d: Value = sqlx::query_scalar("SELECT definition FROM segments WHERE id = $1 AND account_id = $2")
                .bind(id)
                .bind(account_id)
                .fetch_optional(&state.db)
                .await?
                .ok_or_else(|| AppError::not_found("segment"))?;
            Some(segments::load(&state.db, account_id, &d, Some(id)).await?)
        }
        None => None,
    };
    let build = |qb: &mut QueryBuilder<'_, sqlx::Postgres>| {
        qb.push(" FROM profiles p WHERE p.account_id = ").push_bind(account_id);
        if let Some(s) = q.q.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            let like = format!("%{}%", s.replace('%', "\\%").replace('_', "\\_"));
            qb.push(" AND (p.email ILIKE ").push_bind(like.clone())
                .push(" OR p.phone_number ILIKE ").push_bind(like.clone())
                .push(" OR (coalesce(p.first_name,'') || ' ' || coalesce(p.last_name,'')) ILIKE ").push_bind(like.clone())
                .push(" OR p.external_id = ").push_bind(s.to_string())
                .push(")");
        }
        if let Some(l) = q.list_id {
            qb.push(" AND EXISTS (SELECT 1 FROM list_members lm WHERE lm.list_id = ").push_bind(l).push(" AND lm.profile_id = p.id)");
        }
        if let Some(def) = &seg_def {
            qb.push(" AND ");
            segments::push_definition(qb, def);
        }
    };
    let mut cq = QueryBuilder::new("SELECT count(*)");
    build(&mut cq);
    let total: i64 = cq.build_query_scalar().fetch_one(&state.db).await?;
    let mut dq = QueryBuilder::new(format!("SELECT {PROFILE_COLS}"));
    build(&mut dq);
    dq.push(" ORDER BY p.created_at DESC, p.id DESC LIMIT ").push_bind(pg.limit()).push(" OFFSET ").push_bind(pg.offset());
    let rows = dq.build_query_as::<Profile>().fetch_all(&state.db).await?;
    Ok((rows, total))
}

async fn list_profiles_h(State(state): State<AppState>, ctx: Ctx, Query(q): Query<ProfileQuery>) -> AppResult<Json<Value>> {
    let (rows, total) = list_profiles(&state, ctx.account_id, &q).await?;
    Ok(page(rows, total))
}

pub async fn create_profile(state: &AppState, account_id: Uuid, body: Value) -> AppResult<Profile> {
    let list_ids: Vec<Uuid> = body.get("list_ids").and_then(|v| serde_json::from_value(v.clone()).ok()).unwrap_or_default();
    let input: ProfileInput = serde_json::from_value(body)?;
    let up = core::upsert_profile(&state.db, account_id, &input).await?;
    for l in list_ids {
        core::list_belongs(&state.db, account_id, l).await?;
        core::add_to_list(&state.db, account_id, l, &[up.profile.id]).await?;
    }
    Ok(up.profile)
}

async fn create_profile_h(State(state): State<AppState>, ctx: Ctx, ApiJson(b): ApiJson<Value>) -> AppResult<Json<Profile>> {
    Ok(Json(create_profile(&state, ctx.account_id, b).await?))
}

pub async fn load_profile(state: &AppState, account_id: Uuid, id: Uuid) -> AppResult<Profile> {
    let q = format!("SELECT {PROFILE_COLS} FROM profiles p WHERE p.id = $1 AND p.account_id = $2");
    sqlx::query_as::<_, Profile>(&q)
        .bind(id)
        .bind(account_id)
        .fetch_optional(&state.db)
        .await?
        .ok_or_else(|| AppError::not_found("profile"))
}

pub async fn profile_detail(state: &AppState, account_id: Uuid, id: Uuid) -> AppResult<Value> {
    let profile = load_profile(state, account_id, id).await?;
    let lists: Vec<List> = sqlx::query_as(
        "SELECT l.id, l.name, l.double_opt_in, l.show_on_preferences, l.description, (SELECT count(*) FROM list_members x WHERE x.list_id = l.id) AS member_count, l.created_at
         FROM lists l JOIN list_members lm ON lm.list_id = l.id WHERE lm.profile_id = $1 ORDER BY l.name",
    )
    .bind(id)
    .fetch_all(&state.db)
    .await?;
    // Live segment membership (segments are small enough per account to check directly).
    let all: Vec<Segment> = sqlx::query_as("SELECT * FROM segments WHERE account_id = $1 ORDER BY name").bind(account_id).fetch_all(&state.db).await?;
    let mut segs = Vec::new();
    for s in all.into_iter().take(100) {
        if let Ok(def) = segments::load(&state.db, account_id, &s.definition, Some(s.id)).await {
            if segments::profile_matches(&state.db, id, &def).await.unwrap_or(false) {
                segs.push(s);
            }
        }
    }
    let events: Vec<EventRow> = sqlx::query_as(
        "SELECT e.id, m.name AS metric, e.profile_id, e.properties, e.value, e.occurred_at FROM events e JOIN metrics m ON m.id = e.metric_id
         WHERE e.profile_id = $1 ORDER BY e.occurred_at DESC LIMIT 100",
    )
    .bind(id)
    .fetch_all(&state.db)
    .await?;
    #[allow(clippy::type_complexity)]
    let messages: Vec<(Uuid, String, Option<String>, String, Option<Uuid>, Option<Uuid>, Option<DateTime<Utc>>, Option<DateTime<Utc>>, Option<DateTime<Utc>>, Option<String>)> = sqlx::query_as(
        "SELECT id, channel, subject, status, campaign_id, flow_id, sent_at, opened_at, clicked_at, send_note FROM messages WHERE profile_id = $1 AND NOT is_test ORDER BY created_at DESC LIMIT 50",
    )
    .bind(id)
    .fetch_all(&state.db)
    .await?;
    let messages: Vec<Value> = messages
        .into_iter()
        .map(|m| json!({"id": m.0, "channel": m.1, "subject": m.2, "status": m.3, "campaign_id": m.4, "flow_id": m.5, "sent_at": m.6, "opened_at": m.7, "clicked_at": m.8, "note": m.9}))
        .collect();
    Ok(json!({"profile": profile, "lists": lists, "segments": segs, "events": events, "messages": messages}))
}

async fn get_profile_h(State(state): State<AppState>, ctx: Ctx, Path(id): Path<Uuid>) -> AppResult<Json<Value>> {
    Ok(Json(profile_detail(&state, ctx.account_id, id).await?))
}

pub async fn patch_profile(state: &AppState, account_id: Uuid, id: Uuid, mut body: Value) -> AppResult<Profile> {
    load_profile(state, account_id, id).await?;
    body["id"] = json!(id);
    let input: ProfileInput = serde_json::from_value(body)?;
    // Updating by id: identity fields in the body change this profile rather than selecting another.
    let up = core::upsert_profile(&state.db, account_id, &ProfileInput { email: None, phone_number: None, external_id: None, ..input.clone() }).await?;
    let p = up.profile;
    let email = match input.email.as_deref() {
        Some("") => Some(None),
        Some(e) => Some(Some(crate::models::normalize_email(e).ok_or_else(|| AppError::bad("invalid email"))?)),
        None => None,
    };
    let phone = match input.phone_number.as_deref() {
        Some("") => Some(None),
        Some(ph) => Some(Some(crate::models::normalize_phone(ph).ok_or_else(|| AppError::bad("invalid phone number"))?)),
        None => None,
    };
    if email.is_some() || phone.is_some() || input.external_id.is_some() {
        let q = format!(
            "UPDATE profiles p SET email = CASE WHEN $2 THEN $3 ELSE p.email END, phone_number = CASE WHEN $4 THEN $5 ELSE p.phone_number END,
               external_id = COALESCE($6, p.external_id), updated_at = now() WHERE p.id = $1 RETURNING {PROFILE_COLS}"
        );
        return sqlx::query_as::<_, Profile>(&q)
            .bind(id)
            .bind(email.is_some())
            .bind(email.flatten())
            .bind(phone.is_some())
            .bind(phone.flatten())
            .bind(&input.external_id)
            .fetch_one(&state.db)
            .await
            .map_err(|e| match AppError::from(e) {
                AppError::Conflict(_) => AppError::Conflict("another profile already uses that email, phone number or external id".into()),
                other => other,
            });
    }
    Ok(p)
}

async fn patch_profile_h(State(state): State<AppState>, ctx: Ctx, Path(id): Path<Uuid>, ApiJson(b): ApiJson<Value>) -> AppResult<Json<Profile>> {
    Ok(Json(patch_profile(&state, ctx.account_id, id, b).await?))
}

async fn delete_profile_h(State(state): State<AppState>, ctx: Ctx, Path(id): Path<Uuid>) -> AppResult<Json<Value>> {
    let n = sqlx::query("DELETE FROM profiles WHERE id = $1 AND account_id = $2").bind(id).bind(ctx.account_id).execute(&state.db).await?.rows_affected();
    if n == 0 {
        return Err(AppError::not_found("profile"));
    }
    crate::features::team::audit(&state, &ctx, "profile.delete", Some(&id.to_string()), json!({})).await;
    Ok(Json(json!({"ok": true})))
}

#[derive(Deserialize)]
pub struct ImportBody {
    pub csv: String,
    pub list_id: Option<Uuid>,
    pub email_consent: Option<String>,
    pub sms_consent: Option<String>,
    /// The importer confirms everyone on the list agreed to get email from them (required on the
    /// hosted service unless the whole file is imported as unsubscribed).
    #[serde(default)]
    pub permission_confirmed: bool,
}

pub async fn import(state: &AppState, account_id: Uuid, mut b: ImportBody) -> AppResult<Value> {
    let as_unsubscribed = b.email_consent.as_deref() == Some("unsubscribed");
    if state.cfg.hosted && !as_unsubscribed && !b.permission_confirmed {
        return Err(AppError::bad(
            "Confirm that everyone in this file agreed to get email from you (permission_confirmed: true). Bought, rented or scraped lists aren't allowed.",
        ));
    }
    match b.list_id {
        Some(l) => core::list_belongs(&state.db, account_id, l).await?,
        // No list chosen: keep imports together so the first campaign has an obvious audience.
        None => {
            let existing: Option<Uuid> = sqlx::query_scalar("SELECT id FROM lists WHERE account_id = $1 AND name = 'All contacts' LIMIT 1").bind(account_id).fetch_optional(&state.db).await?;
            let id = match existing {
                Some(id) => id,
                None => {
                    let id = Uuid::now_v7();
                    sqlx::query("INSERT INTO lists (id, account_id, name) VALUES ($1, $2, 'All contacts')").bind(id).bind(account_id).execute(&state.db).await?;
                    id
                }
            };
            b.list_id = Some(id);
        }
    }
    if b.csv.len() > 50 * 1024 * 1024 {
        return Err(AppError::bad("CSV is larger than 50 MB; split it into smaller files"));
    }
    // Role and throwaway addresses are left out of marketing imports; an unsubscribe file keeps them.
    let r = core::import_csv(&state.db, account_id, &b.csv, b.list_id, b.email_consent, b.sms_consent, !as_unsubscribed).await?;
    if r.created + r.updated > 0 {
        sqlx::query("UPDATE accounts SET onboarding = onboarding || '{\"contacts\": true}' WHERE id = $1").bind(account_id).execute(&state.db).await?;
    }
    Ok(json!({"created": r.created, "updated": r.updated, "skipped": r.skipped, "errors": r.errors}))
}

async fn import_h(State(state): State<AppState>, ctx: Ctx, ApiJson(b): ApiJson<ImportBody>) -> AppResult<Json<Value>> {
    let v = import(&state, ctx.account_id, b).await?;
    crate::features::team::audit(&state, &ctx, "profiles.import", None, json!({"created": v["created"], "updated": v["updated"], "skipped": v["skipped"]})).await;
    Ok(Json(v))
}

// ---------- Lists ----------

const LIST_SELECT: &str = "SELECT l.id, l.name, l.double_opt_in, l.show_on_preferences, l.description, (SELECT count(*) FROM list_members lm WHERE lm.list_id = l.id) AS member_count, l.created_at FROM lists l";

pub async fn lists(state: &AppState, account_id: Uuid) -> AppResult<Vec<List>> {
    Ok(sqlx::query_as(&format!("{LIST_SELECT} WHERE l.account_id = $1 ORDER BY l.created_at DESC")).bind(account_id).fetch_all(&state.db).await?)
}

pub async fn get_list(state: &AppState, account_id: Uuid, id: Uuid) -> AppResult<List> {
    sqlx::query_as(&format!("{LIST_SELECT} WHERE l.id = $1 AND l.account_id = $2"))
        .bind(id)
        .bind(account_id)
        .fetch_optional(&state.db)
        .await?
        .ok_or_else(|| AppError::not_found("list"))
}

async fn lists_h(State(state): State<AppState>, ctx: Ctx) -> AppResult<Json<Value>> {
    let l = lists(&state, ctx.account_id).await?;
    let n = l.len() as i64;
    Ok(page(l, n))
}

#[derive(Deserialize)]
pub struct ListBody {
    pub name: Option<String>,
    pub double_opt_in: Option<bool>,
    /// Let people join or leave this list themselves on the preference page (from the unsubscribe link).
    pub show_on_preferences: Option<bool>,
    /// A line about the list for the preference page, e.g. "New arrivals every Friday".
    pub description: Option<String>,
}

pub async fn create_list(state: &AppState, account_id: Uuid, b: ListBody) -> AppResult<List> {
    let name = b.name.filter(|n| !n.trim().is_empty()).ok_or_else(|| AppError::bad("give the list a name"))?;
    let id = Uuid::now_v7();
    sqlx::query("INSERT INTO lists (id, account_id, name, double_opt_in, show_on_preferences, description) VALUES ($1, $2, $3, $4, $5, $6)")
        .bind(id)
        .bind(account_id)
        .bind(name.trim())
        .bind(b.double_opt_in.unwrap_or(false))
        .bind(b.show_on_preferences.unwrap_or(false))
        .bind(b.description.as_deref().map(str::trim).unwrap_or(""))
        .execute(&state.db)
        .await?;
    get_list(state, account_id, id).await
}

async fn create_list_h(State(state): State<AppState>, ctx: Ctx, ApiJson(b): ApiJson<ListBody>) -> AppResult<Json<List>> {
    Ok(Json(create_list(&state, ctx.account_id, b).await?))
}

async fn get_list_h(State(state): State<AppState>, ctx: Ctx, Path(id): Path<Uuid>) -> AppResult<Json<List>> {
    Ok(Json(get_list(&state, ctx.account_id, id).await?))
}

/// Rename a list or change its settings (only the fields given change).
pub async fn update_list(state: &AppState, account_id: Uuid, id: Uuid, b: ListBody) -> AppResult<List> {
    get_list(state, account_id, id).await?;
    sqlx::query(
        "UPDATE lists SET name = COALESCE($3, name), double_opt_in = COALESCE($4, double_opt_in),
                show_on_preferences = COALESCE($5, show_on_preferences), description = COALESCE($6, description)
         WHERE id = $1 AND account_id = $2",
    )
    .bind(id)
    .bind(account_id)
    .bind(b.name.filter(|n| !n.trim().is_empty()))
    .bind(b.double_opt_in)
    .bind(b.show_on_preferences)
    .bind(b.description.as_deref().map(str::trim))
    .execute(&state.db)
    .await?;
    get_list(state, account_id, id).await
}

async fn patch_list_h(State(state): State<AppState>, ctx: Ctx, Path(id): Path<Uuid>, ApiJson(b): ApiJson<ListBody>) -> AppResult<Json<List>> {
    Ok(Json(update_list(&state, ctx.account_id, id, b).await?))
}

async fn delete_list_h(State(state): State<AppState>, ctx: Ctx, Path(id): Path<Uuid>) -> AppResult<Json<Value>> {
    let n = sqlx::query("DELETE FROM lists WHERE id = $1 AND account_id = $2").bind(id).bind(ctx.account_id).execute(&state.db).await?.rows_affected();
    if n == 0 {
        return Err(AppError::not_found("list"));
    }
    crate::features::team::audit(&state, &ctx, "list.delete", Some(&id.to_string()), json!({})).await;
    Ok(Json(json!({"ok": true})))
}

#[derive(Deserialize)]
pub struct MembersBody {
    #[serde(default)]
    pub profile_ids: Vec<Uuid>,
    #[serde(default)]
    pub emails: Vec<String>,
}

pub async fn add_members(state: &AppState, account_id: Uuid, list_id: Uuid, b: MembersBody) -> AppResult<u64> {
    core::list_belongs(&state.db, account_id, list_id).await?;
    let mut ids = b.profile_ids;
    for e in b.emails.iter().take(10_000) {
        let up = core::upsert_profile(&state.db, account_id, &ProfileInput { email: Some(e.clone()), ..Default::default() }).await?;
        ids.push(up.profile.id);
    }
    core::add_to_list(&state.db, account_id, list_id, &ids).await
}

async fn add_members_h(State(state): State<AppState>, ctx: Ctx, Path(id): Path<Uuid>, ApiJson(b): ApiJson<MembersBody>) -> AppResult<Json<Value>> {
    Ok(Json(json!({"added": add_members(&state, ctx.account_id, id, b).await?})))
}

async fn remove_members_h(State(state): State<AppState>, ctx: Ctx, Path(id): Path<Uuid>, ApiJson(b): ApiJson<MembersBody>) -> AppResult<Json<Value>> {
    core::list_belongs(&state.db, ctx.account_id, id).await?;
    let n = crate::features::klaviyo_import::remove_members(&state.db, ctx.account_id, id, &b.profile_ids).await?;
    crate::features::team::audit(&state, &ctx, "list.members_remove", Some(&id.to_string()), json!({"removed": n})).await;
    Ok(Json(json!({"removed": n})))
}

// ---------- Segments ----------

pub async fn segments_list(state: &AppState, account_id: Uuid) -> AppResult<Vec<Segment>> {
    Ok(sqlx::query_as("SELECT * FROM segments WHERE account_id = $1 ORDER BY created_at DESC").bind(account_id).fetch_all(&state.db).await?)
}

async fn segments_h(State(state): State<AppState>, ctx: Ctx) -> AppResult<Json<Value>> {
    let s = segments_list(&state, ctx.account_id).await?;
    let n = s.len() as i64;
    Ok(page(s, n))
}

#[derive(Deserialize)]
pub struct SegmentBody {
    pub name: Option<String>,
    pub definition: Option<Value>,
}

pub async fn get_segment(state: &AppState, account_id: Uuid, id: Uuid) -> AppResult<Segment> {
    sqlx::query_as("SELECT * FROM segments WHERE id = $1 AND account_id = $2")
        .bind(id)
        .bind(account_id)
        .fetch_optional(&state.db)
        .await?
        .ok_or_else(|| AppError::not_found("segment"))
}

async fn count_into(state: &AppState, account_id: Uuid, id: Uuid, def: &segments::Definition) -> AppResult<Segment> {
    let (count, _) = segments::preview(&state.db, account_id, def, 0).await?;
    sqlx::query("UPDATE segments SET member_count = $2 WHERE id = $1").bind(id).bind(count as i32).execute(&state.db).await?;
    get_segment(state, account_id, id).await
}

pub async fn create_segment(state: &AppState, account_id: Uuid, b: SegmentBody) -> AppResult<Segment> {
    let name = b.name.filter(|n| !n.trim().is_empty()).ok_or_else(|| AppError::bad("give the segment a name"))?;
    let raw = b.definition.unwrap_or(json!({"match": "all", "conditions": []}));
    let def = segments::load(&state.db, account_id, &raw, None).await?;
    let id = Uuid::now_v7();
    sqlx::query("INSERT INTO segments (id, account_id, name, definition) VALUES ($1, $2, $3, $4)")
        .bind(id)
        .bind(account_id)
        .bind(name.trim())
        .bind(serde_json::to_value(&def)?)
        .execute(&state.db)
        .await?;
    count_into(state, account_id, id, &def).await
}

pub async fn update_segment(state: &AppState, account_id: Uuid, id: Uuid, b: SegmentBody) -> AppResult<Segment> {
    let cur = get_segment(state, account_id, id).await?;
    let def = segments::load(&state.db, account_id, &b.definition.unwrap_or(cur.definition), Some(id)).await?;
    sqlx::query("UPDATE segments SET name = COALESCE($2, name), definition = $3, updated_at = now() WHERE id = $1")
        .bind(id)
        .bind(b.name.filter(|n| !n.trim().is_empty()))
        .bind(serde_json::to_value(&def)?)
        .execute(&state.db)
        .await?;
    count_into(state, account_id, id, &def).await
}

async fn create_segment_h(State(state): State<AppState>, ctx: Ctx, ApiJson(b): ApiJson<SegmentBody>) -> AppResult<Json<Segment>> {
    Ok(Json(create_segment(&state, ctx.account_id, b).await?))
}

async fn get_segment_h(State(state): State<AppState>, ctx: Ctx, Path(id): Path<Uuid>) -> AppResult<Json<Segment>> {
    let s = get_segment(&state, ctx.account_id, id).await?;
    let def = segments::load(&state.db, ctx.account_id, &s.definition, Some(id)).await?;
    Ok(Json(count_into(&state, ctx.account_id, id, &def).await?))
}

async fn patch_segment_h(State(state): State<AppState>, ctx: Ctx, Path(id): Path<Uuid>, ApiJson(b): ApiJson<SegmentBody>) -> AppResult<Json<Segment>> {
    Ok(Json(update_segment(&state, ctx.account_id, id, b).await?))
}

async fn delete_segment_h(State(state): State<AppState>, ctx: Ctx, Path(id): Path<Uuid>) -> AppResult<Json<Value>> {
    let in_use: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM flows WHERE account_id = $1 AND status <> 'draft' AND trigger->>'segment_id' = $2)")
        .bind(ctx.account_id)
        .bind(id.to_string())
        .fetch_one(&state.db)
        .await?;
    if in_use {
        return Err(AppError::Conflict("a live or paused flow is triggered by this segment; change that flow first".into()));
    }
    // Other segments that include this one would stop working.
    let others: Vec<(String, Value)> = sqlx::query_as("SELECT name, definition FROM segments WHERE account_id = $1 AND id <> $2 AND definition::text LIKE '%' || $3 || '%'")
        .bind(ctx.account_id)
        .bind(id)
        .bind(id.to_string())
        .fetch_all(&state.db)
        .await?;
    if let Some((name, _)) = others.iter().find(|(_, d)| segments::referenced_segments(d).contains(&id)) {
        return Err(AppError::Conflict(format!("the segment {name:?} uses this segment; change it first")));
    }
    let flow: Option<String> = sqlx::query_scalar("SELECT name FROM flows WHERE account_id = $1 AND status <> 'draft' AND definition::text LIKE '%' || $2 || '%' LIMIT 1")
        .bind(ctx.account_id)
        .bind(id.to_string())
        .fetch_optional(&state.db)
        .await?;
    if let Some(name) = flow {
        return Err(AppError::Conflict(format!("the flow {name:?} uses this segment in a condition; change it first")));
    }
    let n = sqlx::query("DELETE FROM segments WHERE id = $1 AND account_id = $2").bind(id).bind(ctx.account_id).execute(&state.db).await?.rows_affected();
    if n == 0 {
        return Err(AppError::not_found("segment"));
    }
    crate::features::team::audit(&state, &ctx, "segment.delete", Some(&id.to_string()), json!({})).await;
    Ok(Json(json!({"ok": true})))
}

pub async fn preview(state: &AppState, account_id: Uuid, definition: &Value) -> AppResult<Value> {
    let def = segments::load(&state.db, account_id, definition, None).await?;
    let (count, sample) = segments::preview(&state.db, account_id, &def, 10).await?;
    Ok(json!({"count": count, "sample": sample}))
}

#[derive(Deserialize)]
struct PreviewBody {
    definition: Value,
}

async fn preview_h(State(state): State<AppState>, ctx: Ctx, ApiJson(b): ApiJson<PreviewBody>) -> AppResult<Json<Value>> {
    Ok(Json(preview(&state, ctx.account_id, &b.definition).await?))
}

// ---------- Events and metrics ----------

#[derive(Deserialize)]
pub struct TrackBody {
    pub metric: String,
    pub profile: Value,
    #[serde(default)]
    pub properties: Option<Value>,
    #[serde(default)]
    pub value: Option<f64>,
    #[serde(default)]
    pub time: Option<DateTime<Utc>>,
    #[serde(default)]
    pub unique_id: Option<String>,
}

pub async fn track(state: &AppState, account_id: Uuid, b: TrackBody) -> AppResult<Value> {
    if core::is_server_only_metric(&b.metric) {
        return Err(AppError::bad(format!("{:?} is recorded by DumbMail itself, from a real stock or price change or a confirmed request", b.metric.trim())));
    }
    let input: ProfileInput = serde_json::from_value(b.profile)?;
    let profile = match input.id {
        Some(id) if input.email.is_none() && input.phone_number.is_none() && input.external_id.is_none() => {
            load_profile(state, account_id, id).await?
        }
        _ => core::upsert_profile(&state.db, account_id, &input).await?.profile,
    };
    let props = b.properties.unwrap_or(json!({}));
    if !props.is_object() {
        return Err(AppError::bad("properties must be an object"));
    }
    let id = core::record_event(
        &state.db,
        NewEvent {
            account_id,
            profile_id: profile.id,
            metric: &b.metric,
            properties: props,
            value: b.value,
            occurred_at: b.time,
            unique_id: b.unique_id,
            message_id: None,
        },
    )
    .await?;
    Ok(json!({"id": id, "profile_id": profile.id, "duplicate": id.is_none()}))
}

async fn track_h(State(state): State<AppState>, ctx: Ctx, ApiJson(b): ApiJson<TrackBody>) -> AppResult<Json<Value>> {
    Ok(Json(track(&state, ctx.account_id, b).await?))
}

#[derive(Deserialize)]
pub struct EventsQuery {
    pub metric: Option<String>,
    pub profile_id: Option<Uuid>,
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

pub async fn events(state: &AppState, account_id: Uuid, q: &EventsQuery) -> AppResult<(Vec<EventRow>, i64)> {
    let pg = Page { limit: q.limit, offset: q.offset };
    let total: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM events e JOIN metrics m ON m.id = e.metric_id WHERE e.account_id = $1 AND ($2::text IS NULL OR m.name = $2) AND ($3::uuid IS NULL OR e.profile_id = $3)",
    )
    .bind(account_id)
    .bind(&q.metric)
    .bind(q.profile_id)
    .fetch_one(&state.db)
    .await?;
    let rows: Vec<EventRow> = sqlx::query_as(
        "SELECT e.id, m.name AS metric, e.profile_id, e.properties, e.value, e.occurred_at,
                COALESCE(p.email, p.phone_number, NULLIF(concat_ws(' ', p.first_name, p.last_name), '')) AS profile_label
         FROM events e JOIN metrics m ON m.id = e.metric_id LEFT JOIN profiles p ON p.id = e.profile_id
         WHERE e.account_id = $1 AND ($2::text IS NULL OR m.name = $2) AND ($3::uuid IS NULL OR e.profile_id = $3)
         ORDER BY e.occurred_at DESC LIMIT $4 OFFSET $5",
    )
    .bind(account_id)
    .bind(&q.metric)
    .bind(q.profile_id)
    .bind(pg.limit())
    .bind(pg.offset())
    .fetch_all(&state.db)
    .await?;
    Ok((rows, total))
}

async fn events_h(State(state): State<AppState>, ctx: Ctx, Query(q): Query<EventsQuery>) -> AppResult<Json<Value>> {
    let (rows, total) = events(&state, ctx.account_id, &q).await?;
    Ok(page(rows, total))
}

pub async fn metrics(state: &AppState, account_id: Uuid) -> AppResult<Value> {
    // `trend` is 30 daily counts, oldest first, in rolling 24-hour buckets ending now (for sparklines).
    // Only the last 60 days are scanned (index on account, metric, time); `has_value` looks at those too.
    let rows: Vec<(Uuid, String, i64, i64, Option<DateTime<Utc>>, bool, Vec<i64>)> = sqlx::query_as(
        "SELECT m.id, m.name, s.c30, s.cprev,
                (SELECT max(e.occurred_at) FROM events e WHERE e.account_id = m.account_id AND e.metric_id = m.id),
                s.has_value, t.trend
         FROM metrics m
         CROSS JOIN LATERAL (
            SELECT count(*) FILTER (WHERE e.occurred_at > now() - interval '30 days') AS c30,
                   count(*) FILTER (WHERE e.occurred_at <= now() - interval '30 days') AS cprev,
                   COALESCE(bool_or(e.value IS NOT NULL), false) AS has_value
            FROM events e WHERE e.account_id = m.account_id AND e.metric_id = m.id AND e.occurred_at > now() - interval '60 days') s
         CROSS JOIN LATERAL (
            SELECT array_agg(COALESCE(x.n, 0) ORDER BY d.i) AS trend
            FROM generate_series(0, 29) d(i)
            LEFT JOIN (
                SELECT 29 - floor(extract(epoch FROM now() - e.occurred_at) / 86400)::int AS i, count(*) AS n
                FROM events e WHERE e.account_id = m.account_id AND e.metric_id = m.id AND e.occurred_at > now() - interval '30 days'
                GROUP BY 1) x ON x.i = d.i) t
         WHERE m.account_id = $1 ORDER BY m.name",
    )
    .bind(account_id)
    .fetch_all(&state.db)
    .await?;
    let data: Vec<Value> = rows
        .into_iter()
        .map(|(id, name, c, prev, last, has_value, trend)| {
            json!({"id": id, "name": name, "count_30d": c, "count_prev_30d": prev, "last_seen": last, "has_value": has_value, "trend": trend})
        })
        .collect();
    Ok(json!({"data": data}))
}

async fn metrics_h(State(state): State<AppState>, ctx: Ctx) -> AppResult<Json<Value>> {
    Ok(Json(metrics(&state, ctx.account_id).await?))
}

#[derive(Deserialize)]
pub struct MetricQuery {
    pub metric: String,
    #[serde(default)]
    pub measurement: Option<String>,
    #[serde(default)]
    pub interval: Option<String>,
    #[serde(default)]
    pub start: Option<DateTime<Utc>>,
    #[serde(default)]
    pub end: Option<DateTime<Utc>>,
    #[serde(default)]
    pub group_by: Option<String>,
}

/// Aggregate a metric over time, bucketed in the account's time zone, optionally grouped by an event property.
pub async fn query_metric(state: &AppState, account_id: Uuid, q: MetricQuery) -> AppResult<Value> {
    let interval = match q.interval.as_deref().unwrap_or("day") {
        i @ ("day" | "week" | "month") => i,
        _ => return Err(AppError::bad("interval must be day, week or month")),
    };
    let agg = match q.measurement.as_deref().unwrap_or("count") {
        "count" => "count(*)::float8",
        "unique_profiles" => "count(DISTINCT e.profile_id)::float8",
        "sum_value" => "COALESCE(sum(e.value), 0)::float8",
        _ => return Err(AppError::bad("measurement must be count, unique_profiles or sum_value")),
    };
    let end = q.end.unwrap_or_else(Utc::now);
    let start = q.start.unwrap_or(end - chrono::Duration::days(30));
    if start >= end {
        return Err(AppError::bad("start must be before end"));
    }
    let tz: String = sqlx::query_scalar("SELECT timezone FROM accounts WHERE id = $1").bind(account_id).fetch_one(&state.db).await?;
    let group_expr = match q.group_by.as_deref().filter(|g| !g.is_empty()) {
        Some("campaign") => "COALESCE((SELECT c.name FROM messages mm JOIN campaigns c ON c.id = mm.campaign_id WHERE mm.id = e.message_id), '(none)')".to_string(),
        Some("flow") => "COALESCE((SELECT f.name FROM messages mm JOIN flows f ON f.id = mm.flow_id WHERE mm.id = e.message_id), '(none)')".to_string(),
        Some(_) => "COALESCE(e.properties->>$6, '(none)')".to_string(),
        None => "NULL::text".to_string(),
    };
    let sql = format!(
        "SELECT to_char(date_trunc('{interval}', e.occurred_at AT TIME ZONE $5), 'YYYY-MM-DD') AS bucket, {group_expr} AS grp, {agg} AS value
         FROM events e JOIN metrics m ON m.id = e.metric_id
         WHERE e.account_id = $1 AND m.name = $2 AND e.occurred_at >= $3 AND e.occurred_at < $4
         GROUP BY 1, 2 ORDER BY 1, 2 LIMIT 5000"
    );
    let rows: Vec<(String, Option<String>, f64)> = sqlx::query_as(&sql)
        .bind(account_id)
        .bind(&q.metric)
        .bind(start)
        .bind(end)
        .bind(&tz)
        .bind(q.group_by.clone().unwrap_or_default())
        .fetch_all(&state.db)
        .await?;
    let total_sql = format!("SELECT {agg} FROM events e JOIN metrics m ON m.id = e.metric_id WHERE e.account_id = $1 AND m.name = $2 AND e.occurred_at >= $3 AND e.occurred_at < $4");
    let total: f64 = sqlx::query_scalar(&total_sql).bind(account_id).bind(&q.metric).bind(start).bind(end).fetch_one(&state.db).await?;
    let series: Vec<Value> = rows
        .into_iter()
        .map(|(d, g, v)| if q.group_by.is_some() { json!({"date": d, "group": g, "value": v}) } else { json!({"date": d, "value": v}) })
        .collect();
    Ok(json!({"metric": q.metric, "interval": interval, "start": start, "end": end, "series": series, "total": total}))
}

impl MetricQuery {
    /// Grouping by an event property shows people's own values, so it needs permission to read
    /// profiles; totals and the campaign and flow groupings don't.
    pub fn check_access(&self, ctx: &Ctx) -> AppResult<()> {
        match self.group_by.as_deref().filter(|g| !g.is_empty()) {
            None | Some("campaign" | "flow") => Ok(()),
            Some(_) => crate::features::team::access::require_people_values(ctx, "an event property"),
        }
    }
}

async fn query_metric_h(State(state): State<AppState>, ctx: Ctx, ApiJson(b): ApiJson<MetricQuery>) -> AppResult<Json<Value>> {
    b.check_access(&ctx)?;
    Ok(Json(query_metric(&state, ctx.account_id, b).await?))
}
