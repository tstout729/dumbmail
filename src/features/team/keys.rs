//! API keys with scopes, an optional expiry and the member who made them.

use axum::extract::{Path, State};
use axum::Json;
use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::{json, Value};
use uuid::Uuid;

use super::access::{normalize_scopes, scopes_within};
use super::audit::audit;
use crate::api::ApiJson;
use crate::auth::{self, Ctx, API_KEY_PREFIX};
use crate::error::{AppError, AppResult};
use crate::state::AppState;

pub const MAX_KEYS: i64 = 100;

pub async fn list(state: &AppState, account_id: Uuid) -> AppResult<Value> {
    let rows: Vec<(Uuid, String, String, Vec<String>, DateTime<Utc>, Option<DateTime<Utc>>, Option<DateTime<Utc>>, Option<String>)> = sqlx::query_as(
        "SELECT k.id, k.name, k.prefix, k.scopes, k.created_at, k.last_used_at, k.expires_at, u.email FROM api_keys k LEFT JOIN users u ON u.id = k.created_by
         WHERE k.account_id = $1 AND k.revoked_at IS NULL ORDER BY k.created_at DESC LIMIT 200",
    )
    .bind(account_id)
    .fetch_all(&state.db)
    .await?;
    let now = Utc::now();
    let data: Vec<Value> = rows
        .into_iter()
        .map(|(id, name, prefix, scopes, created_at, last_used_at, expires_at, by)| {
            json!({"id": id, "name": name, "prefix": prefix, "scopes": scopes, "created_at": created_at, "last_used_at": last_used_at,
                   "expires_at": expires_at, "expired": expires_at.is_some_and(|e| e <= now), "created_by": by})
        })
        .collect();
    Ok(json!({"data": data, "scopes": super::access::valid_scopes()}))
}

pub async fn list_h(State(state): State<AppState>, ctx: Ctx) -> AppResult<Json<Value>> {
    Ok(Json(list(&state, ctx.account_id).await?))
}

#[derive(Deserialize, Default)]
pub struct KeyBody {
    pub name: Option<String>,
    /// Defaults to full access.
    pub scopes: Option<Vec<String>>,
    /// Stop working after this many days (1 to 3650). Leave out for no expiry.
    pub expires_in_days: Option<i64>,
}

/// Make a key. The new key acts with the role of the member behind the request. A key made by a
/// key can't have scopes its parent lacks, never expires after its parent, and stops working when
/// its parent is revoked or expires.
pub async fn create(state: &AppState, ctx: &Ctx, b: KeyBody) -> AppResult<Value> {
    let name = b.name.as_deref().map(|n| super::mail::plain(n, 100)).filter(|n| !n.is_empty()).unwrap_or_else(|| "API key".into());
    let scopes = normalize_scopes(&b.scopes.unwrap_or_else(|| vec!["full".into()]))?;
    let (created_by, parent) = if ctx.via_api_key {
        let caller = ctx.scopes.clone().unwrap_or_default();
        if !scopes_within(&scopes, &caller) {
            return Err(AppError::Forbidden("an API key can only make keys with scopes it has itself".into()));
        }
        let by = sqlx::query_scalar::<_, Option<Uuid>>("SELECT created_by FROM api_keys WHERE id = $1").bind(ctx.api_key_id).fetch_one(&state.db).await?;
        (by, ctx.api_key_id)
    } else {
        (ctx.user_id, None)
    };
    if let Some(d) = b.expires_in_days {
        if !(1..=3650).contains(&d) {
            return Err(AppError::bad("expires_in_days must be between 1 and 3650"));
        }
    }
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM api_keys WHERE account_id = $1 AND revoked_at IS NULL").bind(ctx.account_id).fetch_one(&state.db).await?;
    if count >= MAX_KEYS {
        return Err(AppError::bad(format!("an account can have up to {MAX_KEYS} API keys; revoke unused ones first")));
    }
    let key = format!("{API_KEY_PREFIX}{}", auth::random_token(24));
    let prefix: String = key.chars().take(12).collect();
    let id = Uuid::now_v7();
    // The parent's expiry caps the new key's (LEAST skips NULLs, so "no expiry" takes the parent's).
    // The parent must still be live as this commits; a parent revoked a moment ago leaves no row.
    let expires_at: Option<Option<DateTime<Utc>>> = sqlx::query_scalar(
        "INSERT INTO api_keys (id, account_id, name, prefix, key_hash, scopes, expires_at, created_by, parent_key_id)
         SELECT $1, $2, $3, $4, $5, $6,
                LEAST(CASE WHEN $7::bigint IS NULL THEN NULL ELSE now() + make_interval(days => $7::int) END, p.expires_at), $8, $9
         FROM (SELECT 1) one LEFT JOIN api_keys p ON p.id = $9 AND p.account_id = $2
         WHERE $9::uuid IS NULL OR (p.id IS NOT NULL AND p.revoked_at IS NULL AND (p.expires_at IS NULL OR p.expires_at > now()))
         RETURNING expires_at",
    )
    .bind(id)
    .bind(ctx.account_id)
    .bind(&name)
    .bind(&prefix)
    .bind(auth::sha256_hex(&key))
    .bind(&scopes)
    .bind(b.expires_in_days)
    .bind(created_by)
    .bind(parent)
    .fetch_optional(&state.db)
    .await?;
    let expires_at = expires_at.ok_or(AppError::Unauthorized)?;
    audit(state, ctx, "api_key.create", Some(&name), json!({"key_id": id, "key_prefix": prefix, "scopes": scopes, "expires_at": expires_at, "parent_key_id": parent})).await;
    Ok(json!({"id": id, "name": name, "prefix": prefix, "scopes": scopes, "expires_at": expires_at, "key": key}))
}

pub async fn create_h(State(state): State<AppState>, ctx: Ctx, ApiJson(b): ApiJson<KeyBody>) -> AppResult<Json<Value>> {
    Ok(Json(create(&state, &ctx, b).await?))
}

pub async fn revoke(state: &AppState, ctx: &Ctx, id: Uuid) -> AppResult<Value> {
    let mut tx = state.db.begin().await?;
    let row: Option<(String, String)> = sqlx::query_as("UPDATE api_keys SET revoked_at = now() WHERE id = $1 AND account_id = $2 AND revoked_at IS NULL RETURNING name, prefix")
        .bind(id)
        .bind(ctx.account_id)
        .fetch_optional(&mut *tx)
        .await?;
    let (name, prefix) = row.ok_or_else(|| AppError::not_found("API key"))?;
    let children = revoke_descendants(&mut tx, id).await?;
    tx.commit().await?;
    audit(state, ctx, "api_key.revoke", Some(&name), json!({"key_id": id, "key_prefix": prefix, "keys_it_made_revoked": children})).await;
    Ok(json!({"ok": true, "keys_it_made_revoked": children}))
}

/// Revoke every key made (directly or further down) by key `id`. Keys check their parents on
/// every call too; this just makes the dashboard show them as gone.
pub async fn revoke_descendants(tx: &mut sqlx::PgConnection, id: Uuid) -> AppResult<u64> {
    Ok(sqlx::query(
        "WITH RECURSIVE down(id, depth) AS (
           SELECT k.id, 1 FROM api_keys k WHERE k.parent_key_id = $1
           UNION ALL SELECT k.id, down.depth + 1 FROM api_keys k JOIN down ON k.parent_key_id = down.id WHERE down.depth < 50
         )
         UPDATE api_keys SET revoked_at = now() WHERE id IN (SELECT id FROM down) AND revoked_at IS NULL",
    )
    .bind(id)
    .execute(&mut *tx)
    .await?
    .rows_affected())
}

pub async fn revoke_h(State(state): State<AppState>, ctx: Ctx, Path(id): Path<Uuid>) -> AppResult<Json<Value>> {
    Ok(Json(revoke(&state, &ctx, id).await?))
}
