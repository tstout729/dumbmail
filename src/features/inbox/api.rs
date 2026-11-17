//! REST routes for the dashboard and API clients, all under /api/inbox.

use axum::extract::{DefaultBodyLimit, Path, Query, State};
use axum::http::header;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::{json, Value};
use uuid::Uuid;

use super::{conversations, keywords, notify, replies, settings};
use crate::api::ApiJson;
use crate::auth::Ctx;
use crate::error::{AppError, AppResult};
use crate::state::AppState;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/inbox/conversations", get(list_h))
        .route("/api/inbox/unread", get(unread_h))
        .route("/api/inbox/conversations/{id}", get(get_h))
        .route("/api/inbox/conversations/{id}/read", post(read_h))
        .route("/api/inbox/conversations/{id}/status", post(status_h))
        .route("/api/inbox/conversations/{id}/reply", post(reply_h))
        .route("/api/inbox/profiles/{profile_id}", get(for_profile_h))
        .route("/api/inbox/keywords", get(keywords_h).post(create_keyword_h))
        .route("/api/inbox/keywords/{id}", axum::routing::patch(update_keyword_h).delete(delete_keyword_h))
        .route("/api/inbox/saved-replies", get(saved_h).post(create_saved_h))
        .route("/api/inbox/saved-replies/{id}", axum::routing::patch(update_saved_h).delete(delete_saved_h))
        .route("/api/inbox/settings", get(settings_h).patch(update_settings_h))
        .route("/api/inbox/media/{id}", get(media_h))
        .layer(DefaultBodyLimit::max(64 * 1024))
}

async fn list_h(State(state): State<AppState>, ctx: Ctx, Query(q): Query<conversations::ListQuery>) -> AppResult<Json<Value>> {
    let (rows, total) = conversations::list(&state.db, ctx.account_id, &q).await?;
    Ok(crate::api::page(rows, total))
}

async fn unread_h(State(state): State<AppState>, ctx: Ctx) -> AppResult<Json<Value>> {
    Ok(Json(conversations::unread(&state.db, ctx.account_id).await?))
}

#[derive(Deserialize)]
struct ThreadQuery {
    before: Option<DateTime<Utc>>,
    limit: Option<i64>,
}

async fn get_h(State(state): State<AppState>, ctx: Ctx, Path(id): Path<Uuid>, Query(q): Query<ThreadQuery>) -> AppResult<Json<Value>> {
    Ok(Json(conversations::get(&state.db, ctx.account_id, id, q.before, q.limit).await?))
}

async fn for_profile_h(State(state): State<AppState>, ctx: Ctx, Path(profile_id): Path<Uuid>) -> AppResult<Json<Value>> {
    Ok(Json(json!({"conversation_id": conversations::for_profile(&state.db, ctx.account_id, profile_id).await?})))
}

async fn read_h(State(state): State<AppState>, ctx: Ctx, Path(id): Path<Uuid>) -> AppResult<Json<Value>> {
    conversations::mark_read(&state.db, ctx.account_id, id).await?;
    Ok(Json(json!({"ok": true})))
}

#[derive(Deserialize)]
struct StatusBody {
    status: String,
}

async fn status_h(State(state): State<AppState>, ctx: Ctx, Path(id): Path<Uuid>, ApiJson(b): ApiJson<StatusBody>) -> AppResult<Json<Value>> {
    conversations::set_status(&state.db, ctx.account_id, id, &b.status).await?;
    Ok(Json(json!({"ok": true, "status": b.status})))
}

#[derive(Deserialize)]
struct ReplyBody {
    body: String,
    #[serde(default)]
    confirm: bool,
}

async fn reply_h(State(state): State<AppState>, ctx: Ctx, Path(id): Path<Uuid>, ApiJson(b): ApiJson<ReplyBody>) -> AppResult<Json<Value>> {
    Ok(Json(replies::send_reply(&state, ctx.account_id, id, &b.body, ctx.user_id, b.confirm).await?))
}

async fn keywords_h(State(state): State<AppState>, ctx: Ctx) -> AppResult<Json<Value>> {
    let rows = keywords::list(&state.db, ctx.account_id).await?;
    let n = rows.len() as i64;
    Ok(crate::api::page(rows, n))
}

async fn create_keyword_h(State(state): State<AppState>, ctx: Ctx, ApiJson(v): ApiJson<Value>) -> AppResult<Json<Value>> {
    let account = crate::campaigns::load_account(&state.db, ctx.account_id).await?;
    Ok(Json(json!(keywords::upsert(&state.db, &account, None, &v).await?)))
}

async fn update_keyword_h(State(state): State<AppState>, ctx: Ctx, Path(id): Path<Uuid>, ApiJson(v): ApiJson<Value>) -> AppResult<Json<Value>> {
    let account = crate::campaigns::load_account(&state.db, ctx.account_id).await?;
    Ok(Json(json!(keywords::upsert(&state.db, &account, Some(id), &v).await?)))
}

async fn delete_keyword_h(State(state): State<AppState>, ctx: Ctx, Path(id): Path<Uuid>) -> AppResult<Json<Value>> {
    keywords::delete(&state.db, ctx.account_id, id).await?;
    Ok(Json(json!({"ok": true})))
}

async fn saved_h(State(state): State<AppState>, ctx: Ctx) -> AppResult<Json<Value>> {
    let rows = settings::saved_replies(&state.db, ctx.account_id).await?;
    let n = rows.len() as i64;
    Ok(crate::api::page(rows, n))
}

async fn create_saved_h(State(state): State<AppState>, ctx: Ctx, ApiJson(v): ApiJson<Value>) -> AppResult<Json<Value>> {
    let account = crate::campaigns::load_account(&state.db, ctx.account_id).await?;
    Ok(Json(json!(settings::upsert_saved_reply(&state.db, &account, None, &v).await?)))
}

async fn update_saved_h(State(state): State<AppState>, ctx: Ctx, Path(id): Path<Uuid>, ApiJson(v): ApiJson<Value>) -> AppResult<Json<Value>> {
    let account = crate::campaigns::load_account(&state.db, ctx.account_id).await?;
    Ok(Json(json!(settings::upsert_saved_reply(&state.db, &account, Some(id), &v).await?)))
}

async fn delete_saved_h(State(state): State<AppState>, ctx: Ctx, Path(id): Path<Uuid>) -> AppResult<Json<Value>> {
    settings::delete_saved_reply(&state.db, ctx.account_id, id).await?;
    Ok(Json(json!({"ok": true})))
}

async fn settings_h(State(state): State<AppState>, ctx: Ctx) -> AppResult<Json<Value>> {
    Ok(Json(settings::settings_json(&state.db, ctx.account_id).await?))
}

async fn update_settings_h(State(state): State<AppState>, ctx: Ctx, ApiJson(v): ApiJson<Value>) -> AppResult<Json<Value>> {
    let account = crate::campaigns::load_account(&state.db, ctx.account_id).await?;
    settings::update(&state.db, &account, &v).await?;
    Ok(Json(settings::settings_json(&state.db, ctx.account_id).await?))
}

/// An image someone texted in. Only signed-in members of the account can see it.
async fn media_h(State(state): State<AppState>, ctx: Ctx, Path(id): Path<Uuid>) -> AppResult<Response> {
    let (content_type, bytes) = notify::media(&state, ctx.account_id, id).await?.ok_or_else(|| AppError::not_found("attachment"))?;
    Ok((
        [
            (header::CONTENT_TYPE, content_type),
            (header::CACHE_CONTROL, "private, max-age=86400".to_string()),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff".to_string()),
            (header::CONTENT_SECURITY_POLICY, "default-src 'none'; sandbox".to_string()),
        ],
        bytes,
    )
        .into_response())
}
