//! Review photos: checked, re-encoded as JPEG (which drops EXIF and anything else hidden in the
//! file), stored with an unguessable id and served at `/i/rv/{id}.jpg` only while their review is
//! published. The dashboard sees pending photos through short-lived signed addresses.

use axum::extract::{Path, Query, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use uuid::Uuid;

use crate::error::{AppError, AppResult};
use crate::state::AppState;

/// Largest upload taken (before re-encoding).
pub const MAX_UPLOAD: usize = 10 * 1024 * 1024;
/// Most photos on one review.
pub const MAX_PER_REVIEW: i64 = 5;
/// The longest side after re-encoding.
const MAX_SIDE: u32 = 1600;
/// Decoding refuses pictures larger than this (pixels per side), so a tiny file can't claim a huge canvas.
const MAX_DECODE_SIDE: u32 = 12_000;
/// How long a signed address for a photo the public can't see yet works.
const SIGNED_SECS: i64 = 3600;

pub struct Encoded {
    pub bytes: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

/// Decode a PNG, JPEG, GIF or WebP upload (its first frame), shrink it to at most 1600 pixels a
/// side and encode it again as JPEG. Runs off the async threads, with decode limits.
pub async fn reencode(bytes: Vec<u8>) -> AppResult<Encoded> {
    if bytes.len() > MAX_UPLOAD {
        return Err(AppError::bad("photos can be at most 10 MB"));
    }
    if crate::api::images::sniff(&bytes).is_none() {
        return Err(AppError::bad("photos must be PNG, JPEG, GIF or WebP images"));
    }
    tokio::task::spawn_blocking(move || -> AppResult<Encoded> {
        let mut reader = image::ImageReader::new(std::io::Cursor::new(&bytes)).with_guessed_format().map_err(|_| AppError::bad("that photo couldn't be read"))?;
        let mut limits = image::Limits::default();
        limits.max_image_width = Some(MAX_DECODE_SIDE);
        limits.max_image_height = Some(MAX_DECODE_SIDE);
        limits.max_alloc = Some(256 * 1024 * 1024);
        reader.limits(limits);
        let img = reader.decode().map_err(|_| AppError::bad("that photo couldn't be read"))?;
        let img = if img.width() > MAX_SIDE || img.height() > MAX_SIDE { img.resize(MAX_SIDE, MAX_SIDE, image::imageops::FilterType::Triangle) } else { img };
        let rgb = img.to_rgb8();
        let mut out = Vec::new();
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 82).encode_image(&rgb).map_err(|e| AppError::Internal(anyhow::anyhow!("jpeg encode: {e}")))?;
        Ok(Encoded { width: rgb.width(), height: rgb.height(), bytes: out })
    })
    .await
    .map_err(|e| AppError::Internal(anyhow::anyhow!("photo task: {e}")))?
}

/// The public address of a photo (works only while its review is published).
pub fn public_url(base: &str, id: Uuid) -> String {
    format!("{base}/i/rv/{}.jpg", id.simple())
}

/// A signed address that shows the photo whatever its review's status, for an hour (moderation).
pub fn signed_url(state: &AppState, id: Uuid) -> String {
    let exp = chrono::Utc::now().timestamp() + SIGNED_SECS;
    let sig = crate::auth::sign(&state.cfg.secret_key, &format!("rvp{}{exp}", id.simple()));
    format!("{}/i/rv/{}.jpg?exp={exp}&sig={sig}", state.cfg.public_url(), id.simple())
}

fn signed_ok(state: &AppState, id: Uuid, exp: i64, sig: &str) -> bool {
    exp > chrono::Utc::now().timestamp() && crate::auth::verify(&state.cfg.secret_key, sig).as_deref() == Some(format!("rvp{}{exp}", id.simple()).as_str())
}

#[derive(Deserialize)]
pub struct SignedQuery {
    exp: Option<i64>,
    sig: Option<String>,
}

pub async fn serve(State(state): State<AppState>, Path(file): Path<String>, Query(q): Query<SignedQuery>) -> Response {
    let Some(id) = file.strip_suffix(".jpg").and_then(|s| Uuid::parse_str(s).ok()) else { return StatusCode::NOT_FOUND.into_response() };
    let signed = matches!((q.exp, q.sig.as_deref()), (Some(e), Some(s)) if signed_ok(&state, id, e, s));
    let row: Result<Option<Vec<u8>>, sqlx::Error> = sqlx::query_scalar(
        "SELECT ph.bytes FROM review_photos ph LEFT JOIN reviews r ON r.id = ph.review_id
         WHERE ph.id = $1 AND ($2 OR r.status = 'published')",
    )
    .bind(id)
    .bind(signed)
    .fetch_optional(&state.db)
    .await;
    match row {
        Ok(Some(bytes)) => (
            [
                (header::CONTENT_TYPE, "image/jpeg"),
                // Short, so a photo removed or a review unpublished stops showing soon after.
                (header::CACHE_CONTROL, if signed { "private, max-age=300" } else { "public, max-age=300" }),
                (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
            ],
            bytes,
        )
            .into_response(),
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(e) => {
            tracing::error!(error = %e, "review photo read failed");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn reencodes_and_drops_metadata() {
        let mut png = Vec::new();
        let img = image::RgbImage::from_pixel(2400, 1200, image::Rgb([200, 30, 30]));
        image::DynamicImage::ImageRgb8(img).write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png).unwrap();
        let e = reencode(png).await.unwrap();
        assert_eq!((e.width, e.height), (1600, 800));
        assert!(e.bytes.starts_with(&[0xff, 0xd8, 0xff]));
        assert!(!e.bytes.windows(4).any(|w| w == b"Exif"));
        assert!(reencode(b"<svg/>".to_vec()).await.is_err());
        assert!(reencode(b"\x89PNG\r\n\x1a\nnot really".to_vec()).await.is_err());
    }
}
