//! REST routes for object types and objects. MCP tools call the same functions (see `mod.rs`).

use axum::extract::{DefaultBodyLimit, Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};
use uuid::Uuid;

use super::store::{self, UpsertInput, Writer};
use super::types::{self, TypeInput};
use super::{bulk, flow};
use crate::api::ApiJson;
use crate::auth::Ctx;
use crate::error::{AppError, AppResult};
use crate::features::team::access::{rule, Perm, RouteRule};
use crate::features::team::audit;
use crate::state::AppState;

/// Reading types and objects: audience read (linked people and sensitive fields also need profile
/// read, checked in the handler). Writing objects: audience write. Changing or deleting types:
/// settings write. A person's objects: profile read.
pub static ROUTES: &[RouteRule] = &[
    rule("GET", "/api/object-types", Perm::AudienceRead),
    rule("POST", "/api/object-types", Perm::SettingsWrite),
    rule("GET", "/api/object-types/{key}", Perm::AudienceRead),
    rule("PATCH", "/api/object-types/{key}", Perm::SettingsWrite),
    rule("DELETE", "/api/object-types/{key}", Perm::SettingsWrite),
    rule("GET", "/api/object-types/{key}/objects", Perm::AudienceRead),
    rule("POST", "/api/objects", Perm::AudienceWrite),
    rule("POST", "/api/objects/bulk", Perm::AudienceWrite),
    rule("GET", "/api/objects/bulk/{id}", Perm::AudienceRead),
    rule("POST", "/api/objects/delete", Perm::AudienceWrite),
    rule("GET", "/api/objects/{id}", Perm::AudienceRead),
    rule("DELETE", "/api/objects/{id}", Perm::AudienceWrite),
    rule("GET", "/api/profiles/{id}/objects", Perm::ProfilesRead),
];

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/object-types", get(list_types_h).post(upsert_type_h))
        .route("/api/object-types/{key}", get(get_type_h).patch(patch_type_h).delete(delete_type_h))
        .route("/api/object-types/{key}/objects", get(list_objects_h))
        .route("/api/objects", post(upsert_h).layer(DefaultBodyLimit::max(256 * 1024)))
        .route("/api/objects/bulk", post(bulk_h).layer(DefaultBodyLimit::max(bulk::MAX_BYTES + 64 * 1024)))
        .route("/api/objects/bulk/{id}", get(bulk_view_h))
        .route("/api/objects/delete", post(bulk_delete_h))
        .route("/api/objects/{id}", get(get_h).delete(delete_h))
        .route("/api/profiles/{id}/objects", get(profile_objects_h))
}

pub fn type_json(t: &types::ObjectType, count: Option<i64>) -> Value {
    let mut v = json!(t);
    // A sensitive field's options are never listed (they'd say what values it can hold).
    if let Some(fields) = v["fields"].as_array_mut() {
        for f in fields {
            if f["sensitive"] == true && f.get("options").is_some() {
                f.as_object_mut().map(|o| o.remove("options"));
                f["options_hidden"] = json!(true);
            }
        }
    }
    if let Some(n) = count {
        v["object_count"] = json!(n);
    }
    v
}

pub async fn list_types(state: &AppState, account_id: Uuid) -> AppResult<Value> {
    let rows = types::list(&state.db, account_id).await?;
    Ok(json!({"data": rows.iter().map(|(t, n)| type_json(t, Some(*n))).collect::<Vec<_>>()}))
}

pub async fn upsert_type(state: &AppState, ctx: &Ctx, b: TypeInput) -> AppResult<Value> {
    let (t, created) = types::upsert(&state.db, ctx.account_id, b).await?;
    audit(state, ctx, if created { "object_type.create" } else { "object_type.update" }, Some(&t.key), json!({"fields": t.fields.len()})).await;
    Ok(type_json(&t, None))
}

/// What deleting a type would do; refused while a flow or segment uses it.
pub async fn delete_type(state: &AppState, ctx: &Ctx, key: &str, confirm: bool) -> AppResult<Value> {
    let t = types::require(&state.db, ctx.account_id, key).await?;
    let users = types::users(&state.db, ctx.account_id, &t.key).await?;
    if !users.is_empty() {
        return Err(AppError::Conflict(format!("{} is used by {}; change those first", t.key, users.join(", "))));
    }
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM objects WHERE type_id = $1").bind(t.id).fetch_one(&state.db).await?;
    if !confirm {
        return Ok(json!({"dry_run": true, "objects": n, "note": format!(
            "Nothing deleted. This permanently deletes the {} type and its {n} objects, with their history and links to people (the people stay). Call again with confirm=true once the user approves.", t.name)}));
    }
    types::delete(&state.db, &t).await?;
    audit(state, ctx, "object_type.delete", Some(&t.key), json!({"objects": n})).await;
    Ok(json!({"deleted": true, "objects": n}))
}

pub async fn upsert_one(state: &AppState, ctx: &Ctx, source: &str, b: &UpsertInput) -> AppResult<Value> {
    Ok(json!(store::upsert(state, &Writer::new(ctx.account_id, source), b).await?))
}

/// Delete objects (by id, or by type and external ids). Without `confirm`, says what would go.
pub async fn delete_objects(state: &AppState, ctx: &Ctx, ids: Vec<Uuid>, confirm: bool) -> AppResult<Value> {
    if ids.is_empty() || ids.len() > 500 {
        return Err(AppError::bad("give 1 to 500 objects to delete"));
    }
    let found: Vec<(Uuid, String, String)> = sqlx::query_as("SELECT o.id, t.key, o.external_id FROM objects o JOIN object_types t ON t.id = o.type_id WHERE o.account_id = $1 AND o.id = ANY($2)")
        .bind(ctx.account_id)
        .bind(&ids)
        .fetch_all(&state.db)
        .await?;
    if found.is_empty() {
        return Err(AppError::not_found("object"));
    }
    if !confirm {
        return Ok(json!({"dry_run": true, "objects": found.len(), "note": format!(
            "Nothing deleted. This permanently deletes {} object(s) with their history; people in flows for them leave those flows. Call again with confirm=true once the user approves.", found.len())}));
    }
    let ids: Vec<Uuid> = found.iter().map(|(id, _, _)| *id).collect();
    let n = store::delete(&state.db, ctx.account_id, &ids).await?;
    let mut keys: Vec<&str> = found.iter().map(|(_, k, _)| k.as_str()).collect();
    keys.sort();
    keys.dedup();
    let target = if found.len() == 1 { Some(found[0].0.to_string()) } else { None };
    audit(state, ctx, "objects.delete", target.as_deref(), json!({"count": n, "types": keys})).await;
    Ok(json!({"deleted": n}))
}

// ---------- Handlers ----------

async fn list_types_h(State(state): State<AppState>, ctx: Ctx) -> AppResult<Json<Value>> {
    Ok(Json(list_types(&state, ctx.account_id).await?))
}

async fn upsert_type_h(State(state): State<AppState>, ctx: Ctx, ApiJson(b): ApiJson<TypeInput>) -> AppResult<Json<Value>> {
    Ok(Json(upsert_type(&state, &ctx, b).await?))
}

async fn get_type_h(State(state): State<AppState>, ctx: Ctx, Path(key): Path<String>) -> AppResult<Json<Value>> {
    let t = types::require(&state.db, ctx.account_id, &key).await?;
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM objects WHERE type_id = $1").bind(t.id).fetch_one(&state.db).await?;
    let mut v = type_json(&t, Some(n));
    v["used_by"] = json!(types::users(&state.db, ctx.account_id, &t.key).await?);
    Ok(Json(v))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TypePatch {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    plural: Option<String>,
    #[serde(default)]
    fields: Option<Vec<types::FieldDef>>,
    #[serde(default)]
    title_field: Option<String>,
}

async fn patch_type_h(State(state): State<AppState>, ctx: Ctx, Path(key): Path<String>, ApiJson(b): ApiJson<TypePatch>) -> AppResult<Json<Value>> {
    types::require(&state.db, ctx.account_id, &key).await?;
    let input = TypeInput { key, name: b.name, plural: b.plural, fields: b.fields, title_field: b.title_field };
    Ok(Json(upsert_type(&state, &ctx, input).await?))
}

#[derive(Deserialize)]
struct ConfirmQuery {
    confirm: Option<bool>,
}

async fn delete_type_h(State(state): State<AppState>, ctx: Ctx, Path(key): Path<String>, Query(q): Query<ConfirmQuery>) -> AppResult<Json<Value>> {
    let v = delete_type(&state, &ctx, &key, q.confirm == Some(true)).await?;
    if v["dry_run"] == true {
        return Err(AppError::bad(format!("{} (REST: DELETE /api/object-types/{key}?confirm=true)", v["note"].as_str().unwrap_or(""))));
    }
    Ok(Json(v))
}

#[derive(Deserialize)]
pub struct ListQuery {
    pub q: Option<String>,
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

async fn list_objects_h(State(state): State<AppState>, ctx: Ctx, Path(key): Path<String>, Query(q): Query<ListQuery>) -> AppResult<Json<Value>> {
    let t = types::require(&state.db, ctx.account_id, &key).await?;
    Ok(Json(store::list(&state.db, &t, q.q.as_deref(), q.limit.unwrap_or(50).clamp(1, 200), q.offset.unwrap_or(0).clamp(0, 1_000_000)).await?))
}

async fn upsert_h(State(state): State<AppState>, ctx: Ctx, ApiJson(b): ApiJson<UpsertInput>) -> AppResult<Json<Value>> {
    Ok(Json(upsert_one(&state, &ctx, "api", &b).await?))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BulkBody {
    objects: Vec<UpsertInput>,
}

async fn bulk_h(State(state): State<AppState>, ctx: Ctx, ApiJson(b): ApiJson<BulkBody>) -> AppResult<(StatusCode, Json<Value>)> {
    let v = bulk::start(&state, ctx.account_id, "api", &b.objects).await?;
    audit(&state, &ctx, "objects.bulk_upsert", v["id"].as_str(), json!({"count": b.objects.len()})).await;
    Ok((StatusCode::ACCEPTED, Json(v)))
}

async fn bulk_view_h(State(state): State<AppState>, ctx: Ctx, Path(id): Path<Uuid>) -> AppResult<Json<Value>> {
    Ok(Json(bulk::view(&state.db, ctx.account_id, id).await?))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeleteBody {
    #[serde(default)]
    pub ids: Vec<Uuid>,
    #[serde(default, rename = "type")]
    pub type_key: Option<String>,
    #[serde(default)]
    pub external_ids: Vec<String>,
    #[serde(default)]
    pub confirm: bool,
}

pub async fn delete_ids(state: &AppState, account_id: Uuid, b: &DeleteBody) -> AppResult<Vec<Uuid>> {
    let mut ids = b.ids.clone();
    if !b.external_ids.is_empty() {
        let key = b.type_key.as_deref().ok_or_else(|| AppError::bad("external_ids go with the object type (type)"))?;
        if b.external_ids.len() > 500 {
            return Err(AppError::bad("give at most 500 external ids"));
        }
        ids.extend(store::ids_for(&state.db, account_id, key, &b.external_ids).await?.into_iter().map(|(id, _)| id));
    }
    Ok(ids)
}

async fn bulk_delete_h(State(state): State<AppState>, ctx: Ctx, ApiJson(b): ApiJson<DeleteBody>) -> AppResult<Json<Value>> {
    let ids = delete_ids(&state, ctx.account_id, &b).await?;
    Ok(Json(delete_objects(&state, &ctx, ids, b.confirm).await?))
}

async fn get_h(State(state): State<AppState>, ctx: Ctx, Path(id): Path<Uuid>) -> AppResult<Json<Value>> {
    Ok(Json(store::detail(&state.db, ctx.account_id, id, ctx.can(Perm::ProfilesRead)).await?))
}

async fn delete_h(State(state): State<AppState>, ctx: Ctx, Path(id): Path<Uuid>) -> AppResult<StatusCode> {
    delete_objects(&state, &ctx, vec![id], true).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn profile_objects_h(State(state): State<AppState>, ctx: Ctx, Path(id): Path<Uuid>) -> AppResult<Json<Value>> {
    Ok(Json(store::for_profile(&state.db, ctx.account_id, id).await?))
}

/// The object-date report for a flow's results (used by the flow report).
pub async fn flow_report(state: &AppState, f: &crate::models::FlowRow) -> AppResult<Value> {
    flow::report(&state.db, f).await
}
