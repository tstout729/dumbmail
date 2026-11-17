//! Uploaded images for forms and emails. Stored in Postgres and served publicly at /i/{id}.

use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{header, HeaderMap};
use axum::routing::{delete, get};
use axum::{Json, Router};
use base64::Engine;
use chrono::{DateTime, Utc};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use super::page;
use crate::auth::Ctx;
use crate::error::{AppError, AppResult};
use crate::state::AppState;

pub const MAX_BYTES: usize = 5 * 1024 * 1024;
pub const MAX_PER_ACCOUNT: i64 = 500;

pub fn routes() -> Router<AppState> {
    Router::new()
        // Room for a 5 MB file sent as base64 JSON.
        .route("/api/images", get(list_h).post(upload_h).layer(axum::extract::DefaultBodyLimit::max(8 * 1024 * 1024)))
        .route("/api/images/{id}", delete(delete_h))
}

/// The image type from the file's first bytes. The type the uploader claims is ignored, and SVG
/// (which can carry scripts) isn't accepted.
pub fn sniff(b: &[u8]) -> Option<&'static str> {
    if b.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if b.starts_with(&[0xff, 0xd8, 0xff]) {
        Some("image/jpeg")
    } else if b.starts_with(b"GIF87a") || b.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if b.len() >= 12 && &b[..4] == b"RIFF" && &b[8..12] == b"WEBP" {
        Some("image/webp")
    } else {
        None
    }
}

#[derive(sqlx::FromRow)]
struct ImageRow {
    id: Uuid,
    content_type: String,
    size: i32,
    created_at: DateTime<Utc>,
}

fn image_json(state: &AppState, r: &ImageRow) -> Value {
    json!({"id": r.id, "url": format!("{}/i/{}", state.cfg.public_url(), r.id), "content_type": r.content_type, "size": r.size, "created_at": r.created_at})
}

fn too_big(n: usize) -> AppError {
    AppError::bad(format!("images can be at most 5 MB; this one is {:.1} MB", n as f64 / 1_048_576.0))
}

async fn existing(state: &AppState, account_id: Uuid, sha: &str) -> AppResult<Option<ImageRow>> {
    Ok(sqlx::query_as("SELECT id, content_type, size, created_at FROM images WHERE account_id = $1 AND sha256 = $2").bind(account_id).bind(sha).fetch_optional(&state.db).await?)
}

/// Store an image for the account, or return the one it already has with the same bytes.
pub async fn store(state: &AppState, account_id: Uuid, bytes: &[u8]) -> AppResult<Value> {
    if bytes.is_empty() {
        return Err(AppError::bad("the image is empty"));
    }
    if bytes.len() > MAX_BYTES {
        return Err(too_big(bytes.len()));
    }
    let content_type = sniff(bytes).ok_or_else(|| AppError::bad("that file isn't a PNG, JPEG, GIF or WebP image (SVG and other types aren't accepted)"))?;
    let sha = hex::encode(Sha256::digest(bytes));
    if let Some(r) = existing(state, account_id, &sha).await? {
        return Ok(image_json(state, &r));
    }
    // Count and insert under a per-account lock, so simultaneous uploads can't pass the limit together.
    let mut tx = state.db.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1::text, 18))").bind(account_id).execute(&mut *tx).await?;
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM images WHERE account_id = $1").bind(account_id).fetch_one(&mut *tx).await?;
    if count >= MAX_PER_ACCOUNT {
        return Err(AppError::Conflict(format!(
            "this account already stores {MAX_PER_ACCOUNT} images, the most it can keep. Delete ones you no longer use (DELETE /api/images/:id, or the MCP delete tool with type \"image\") and upload again."
        )));
    }
    let inserted: Option<ImageRow> = sqlx::query_as(
        "INSERT INTO images (id, account_id, content_type, size, sha256, bytes) VALUES ($1, $2, $3, $4, $5, $6)
         ON CONFLICT (account_id, sha256) DO NOTHING RETURNING id, content_type, size, created_at",
    )
    .bind(Uuid::now_v7())
    .bind(account_id)
    .bind(content_type)
    .bind(bytes.len() as i32)
    .bind(&sha)
    .bind(bytes)
    .fetch_optional(&mut *tx)
    .await?;
    tx.commit().await?;
    // A simultaneous upload of the same file won the insert: return that one.
    let r = match inserted {
        Some(r) => r,
        None => existing(state, account_id, &sha).await?.ok_or_else(|| AppError::Conflict("could not save the image, please retry".into()))?,
    };
    Ok(image_json(state, &r))
}

/// `{url}` copies a public image from the web; `{data_base64}` (optionally a data: URL) uploads one inline.
pub async fn store_json(state: &AppState, account_id: Uuid, v: &Value) -> AppResult<Value> {
    if let Some(u) = v["url"].as_str().map(str::trim).filter(|u| !u.is_empty()) {
        if !(u.starts_with("https://") || u.starts_with("http://")) {
            return Err(AppError::bad("url must be an http(s) link to the image"));
        }
        let fetched = crate::flows::fetch_public(u, MAX_BYTES + 1).await?;
        if fetched.body.len() > MAX_BYTES {
            return Err(AppError::bad("that image is bigger than 5 MB"));
        }
        return store(state, account_id, &fetched.body).await;
    }
    if let Some(d) = v["data_base64"].as_str().filter(|d| !d.trim().is_empty()) {
        let d = d.trim();
        let d = if d.starts_with("data:") { d.split_once(',').map(|(_, b)| b).unwrap_or("") } else { d };
        let clean: String = d.chars().filter(|c| !c.is_ascii_whitespace()).collect();
        if clean.len() > MAX_BYTES / 3 * 4 + 8 {
            return Err(too_big(clean.len() / 4 * 3));
        }
        use base64::engine::{general_purpose::GeneralPurpose, DecodePaddingMode, GeneralPurposeConfig};
        let lenient = GeneralPurposeConfig::new().with_decode_padding_mode(DecodePaddingMode::Indifferent);
        let bytes = GeneralPurpose::new(&base64::alphabet::STANDARD, lenient)
            .decode(&clean)
            .or_else(|_| GeneralPurpose::new(&base64::alphabet::URL_SAFE, lenient).decode(&clean))
            .map_err(|_| AppError::bad("data_base64 isn't valid base64"))?;
        return store(state, account_id, &bytes).await;
    }
    Err(AppError::bad("send {\"url\": \"https://…\"} to copy an image from the web, or {\"data_base64\": \"…\"} with the file's contents"))
}

pub async fn list(state: &AppState, account_id: Uuid) -> AppResult<(Vec<Value>, i64)> {
    let rows: Vec<ImageRow> = sqlx::query_as("SELECT id, content_type, size, created_at FROM images WHERE account_id = $1 ORDER BY created_at DESC, id DESC LIMIT 200")
        .bind(account_id)
        .fetch_all(&state.db)
        .await?;
    let total: i64 = sqlx::query_scalar("SELECT count(*) FROM images WHERE account_id = $1").bind(account_id).fetch_one(&state.db).await?;
    Ok((rows.iter().map(|r| image_json(state, r)).collect(), total))
}

pub async fn remove(state: &AppState, account_id: Uuid, id: Uuid) -> AppResult<()> {
    let n = sqlx::query("DELETE FROM images WHERE id = $1 AND account_id = $2").bind(id).bind(account_id).execute(&state.db).await?.rows_affected();
    if n == 0 {
        return Err(AppError::not_found("image"));
    }
    Ok(())
}

async fn upload_h(State(state): State<AppState>, ctx: Ctx, headers: HeaderMap, body: Bytes) -> AppResult<Json<Value>> {
    let ct = headers.get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).unwrap_or("").to_ascii_lowercase();
    if ct.starts_with("application/json") {
        let v: Value = serde_json::from_slice(&body)?;
        return Ok(Json(store_json(&state, ctx.account_id, &v).await?));
    }
    if ct.starts_with("image/") || ct.starts_with("application/octet-stream") {
        return Ok(Json(store(&state, ctx.account_id, &body).await?));
    }
    Err(AppError::bad(
        "send the image file as the request body with its Content-Type (image/png, image/jpeg, image/gif or image/webp), or JSON {\"url\": \"https://…\"} or {\"data_base64\": \"…\"}",
    ))
}

async fn list_h(State(state): State<AppState>, ctx: Ctx) -> AppResult<Json<Value>> {
    let (rows, total) = list(&state, ctx.account_id).await?;
    Ok(page(rows, total))
}

async fn delete_h(State(state): State<AppState>, ctx: Ctx, Path(id): Path<Uuid>) -> AppResult<Json<Value>> {
    remove(&state, ctx.account_id, id).await?;
    crate::features::team::audit(&state, &ctx, "image.delete", Some(&id.to_string()), json!({})).await;
    Ok(Json(json!({"ok": true})))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sniffs_only_raster_images() {
        assert_eq!(sniff(b"\x89PNG\r\n\x1a\nrest"), Some("image/png"));
        assert_eq!(sniff(&[0xff, 0xd8, 0xff, 0xe0]), Some("image/jpeg"));
        assert_eq!(sniff(b"GIF89a..."), Some("image/gif"));
        assert_eq!(sniff(b"RIFF\0\0\0\0WEBPVP8 "), Some("image/webp"));
        assert_eq!(sniff(b"<svg xmlns=\"http://www.w3.org/2000/svg\"/>"), None);
        assert_eq!(sniff(b"hello"), None);
    }
}
