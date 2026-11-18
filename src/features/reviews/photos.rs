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

/// The most pixels a photo may declare (checked from its header, before decoding).
pub const MAX_PIXELS: u64 = 40_000_000;
/// Photo decodes running at once on this server, and for one account.
const DECODES: usize = 4;
const DECODES_PER_ACCOUNT: usize = 2;
/// How long an upload waits for a decode slot before it's turned away.
const DECODE_WAIT: std::time::Duration = std::time::Duration::from_secs(10);

static DECODE_SLOTS: once_cell::sync::Lazy<std::sync::Arc<tokio::sync::Semaphore>> = once_cell::sync::Lazy::new(|| std::sync::Arc::new(tokio::sync::Semaphore::new(DECODES)));
static ACCOUNT_SLOTS: once_cell::sync::Lazy<std::sync::Mutex<std::collections::HashMap<Uuid, std::sync::Arc<tokio::sync::Semaphore>>>> =
    once_cell::sync::Lazy::new(|| std::sync::Mutex::new(std::collections::HashMap::new()));

fn account_slots(account_id: Uuid) -> std::sync::Arc<tokio::sync::Semaphore> {
    let mut m = ACCOUNT_SLOTS.lock().unwrap_or_else(|e| e.into_inner());
    if m.len() > 10_000 {
        m.retain(|_, s| s.available_permits() < DECODES_PER_ACCOUNT);
    }
    m.entry(account_id).or_insert_with(|| std::sync::Arc::new(tokio::sync::Semaphore::new(DECODES_PER_ACCOUNT))).clone()
}

fn busy() -> AppError {
    AppError::Busy("Too many photos are being processed right now. Please try again in a moment.".into())
}

/// The size an image file says it is, read from its header without decoding it.
pub fn declared_size(bytes: &[u8]) -> AppResult<(u32, u32)> {
    image::ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .ok()
        .and_then(|r| r.into_dimensions().ok())
        .ok_or_else(|| AppError::bad("that photo couldn't be read"))
}

/// [`reencode`] for one account: waits for a free decode slot, at most [`DECODES_PER_ACCOUNT`] for
/// the account and [`DECODES`] on the server at once.
pub async fn reencode_for(account_id: Uuid, bytes: Vec<u8>) -> AppResult<Encoded> {
    let mine = tokio::time::timeout(DECODE_WAIT, account_slots(account_id).acquire_owned()).await.map_err(|_| busy())?.map_err(|_| busy())?;
    let all = tokio::time::timeout(DECODE_WAIT, DECODE_SLOTS.clone().acquire_owned()).await.map_err(|_| busy())?.map_err(|_| busy())?;
    let out = reencode(bytes).await;
    drop(all);
    drop(mine);
    out
}

/// Check a PNG, JPEG, GIF or WebP upload, then decode it (its first frame), shrink it to at most
/// 1600 pixels a side and encode it again as JPEG. Its declared size is checked against
/// [`MAX_PIXELS`] before anything is decoded. Runs off the async threads, with decode limits. Use
/// [`reencode_for`] so decodes are bounded.
pub async fn reencode(bytes: Vec<u8>) -> AppResult<Encoded> {
    if bytes.len() > MAX_UPLOAD {
        return Err(AppError::bad("photos can be at most 10 MB"));
    }
    if crate::api::images::sniff(&bytes).is_none() {
        return Err(AppError::bad("photos must be PNG, JPEG, GIF or WebP images"));
    }
    let (w, h) = declared_size(&bytes)?;
    if u64::from(w) * u64::from(h) > MAX_PIXELS || w > MAX_DECODE_SIDE || h > MAX_DECODE_SIDE {
        return Err(AppError::bad(format!("that photo is too large ({w} × {h} pixels); photos can be at most 40 million pixels")));
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

    /// A PNG header that claims `w` × `h` pixels, with no image data behind it.
    fn claims(w: u32, h: u32) -> Vec<u8> {
        let mut ihdr = Vec::new();
        ihdr.extend_from_slice(&w.to_be_bytes());
        ihdr.extend_from_slice(&h.to_be_bytes());
        ihdr.extend_from_slice(&[8, 2, 0, 0, 0]);
        let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
        out.extend_from_slice(&13u32.to_be_bytes());
        out.extend_from_slice(b"IHDR");
        out.extend_from_slice(&ihdr);
        let mut crc_in = b"IHDR".to_vec();
        crc_in.extend_from_slice(&ihdr);
        out.extend_from_slice(&crc32(&crc_in).to_be_bytes());
        out
    }

    fn crc32(b: &[u8]) -> u32 {
        let mut c = 0xffff_ffffu32;
        for &x in b {
            c ^= u32::from(x);
            for _ in 0..8 {
                c = if c & 1 != 0 { 0xedb8_8320 ^ (c >> 1) } else { c >> 1 };
            }
        }
        !c
    }

    #[tokio::test]
    async fn huge_declared_images_are_refused_before_decoding() {
        assert_eq!(declared_size(&claims(9000, 9000)).unwrap(), (9000, 9000));
        let e = reencode(claims(9000, 9000)).await.err().unwrap();
        assert!(e.public_message().contains("too large"), "{}", e.public_message());
    }

    #[tokio::test]
    async fn decodes_per_account_are_bounded() {
        let acct = Uuid::now_v7();
        let a = account_slots(acct).try_acquire_owned().unwrap();
        let b = account_slots(acct).try_acquire_owned().unwrap();
        assert!(account_slots(acct).try_acquire_owned().is_err(), "a third decode for one account waits");
        assert!(account_slots(Uuid::now_v7()).try_acquire_owned().is_ok(), "other accounts aren't held up");
        drop((a, b));
        assert!(account_slots(acct).try_acquire_owned().is_ok());
    }
}
