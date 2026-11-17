//! Video thumbnails: the video's own thumbnail (YouTube, Vimeo) or an image the customer gives,
//! with a play button drawn on it, stored as one of the account's images. Email apps can't play
//! video or lay a button over an image reliably, so the button is part of the picture.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use once_cell::sync::Lazy;
use tokio::sync::Semaphore;

use image::{DynamicImage, GenericImageView, ImageFormat, Rgba, RgbaImage};
use reqwest::Method;
use serde_json::{json, Value};
use uuid::Uuid;

use super::net;
use crate::error::{AppError, AppResult};
use crate::state::AppState;

const MAX_SOURCE: usize = 5 * 1024 * 1024;
/// The most pixels a source image may have (checked from its header, before decoding it).
pub const MAX_PIXELS: u64 = 24_000_000;
/// The largest thumbnail, in either direction.
pub const MAX_OUTPUT: u32 = 1200;
/// Thumbnails being made at once across the server (one at a time per account).
pub const GLOBAL_CONVERSIONS: usize = 2;

static CONVERTING: Lazy<Arc<Semaphore>> = Lazy::new(|| Arc::new(Semaphore::new(GLOBAL_CONVERSIONS)));
static ACCOUNTS: Lazy<std::sync::Mutex<HashSet<Uuid>>> = Lazy::new(|| std::sync::Mutex::new(HashSet::new()));

/// The account's one-thumbnail-at-a-time slot; released when dropped.
struct Slot(Uuid);

impl Drop for Slot {
    fn drop(&mut self) {
        if let Ok(mut a) = ACCOUNTS.lock() {
            a.remove(&self.0);
        }
    }
}

fn take_slot(account_id: Uuid) -> AppResult<Slot> {
    let mut a = ACCOUNTS.lock().map_err(|_| AppError::Busy("try again in a few seconds".into()))?;
    if !a.insert(account_id) {
        return Err(AppError::Busy("a video thumbnail is already being made for this account; try again in a few seconds".into()));
    }
    Ok(Slot(account_id))
}

#[derive(Debug, PartialEq)]
pub enum Video {
    YouTube(String),
    Vimeo(String),
}

/// Recognize a YouTube or Vimeo link. Only the id is kept; the addresses we fetch are built from it.
pub fn parse(raw: &str) -> Option<Video> {
    let u = url::Url::parse(raw.trim()).ok()?;
    if !matches!(u.scheme(), "http" | "https") {
        return None;
    }
    let host = u.host_str()?.trim_start_matches("www.").trim_start_matches("m.").to_ascii_lowercase();
    let segs: Vec<&str> = u.path_segments().map(|s| s.filter(|x| !x.is_empty()).collect()).unwrap_or_default();
    let yt_id = |s: &str| (s.len() == 11 && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')).then(|| s.to_string());
    match host.as_str() {
        "youtube.com" | "music.youtube.com" | "youtube-nocookie.com" => match segs.as_slice() {
            ["watch"] => u.query_pairs().find(|(k, _)| k == "v").and_then(|(_, v)| yt_id(&v)).map(Video::YouTube),
            ["shorts" | "embed" | "live" | "v", id, ..] => yt_id(id).map(Video::YouTube),
            _ => None,
        },
        "youtu.be" => segs.first().and_then(|id| yt_id(id)).map(Video::YouTube),
        "vimeo.com" | "player.vimeo.com" => segs.iter().rev().find(|s| !s.is_empty() && s.len() <= 12 && s.chars().all(|c| c.is_ascii_digit())).map(|id| Video::Vimeo(id.to_string())),
        _ => None,
    }
}

async fn get(url: &str, max: usize) -> AppResult<net::Response> {
    let r = net::request(Method::GET, url, max + 1, Duration::from_secs(10)).await.map_err(|e| AppError::bad(format!("that image address {}", e.describe())))?;
    if r.body.len() > max {
        return Err(AppError::bad("the thumbnail is bigger than 5 MB"));
    }
    Ok(r)
}

/// The video's own thumbnail image bytes.
async fn provider_thumbnail(v: &Video) -> AppResult<Vec<u8>> {
    match v {
        Video::YouTube(id) => {
            // The largest size exists only for HD uploads; the smaller one always does.
            for size in ["maxresdefault", "hqdefault"] {
                let r = get(&format!("https://i.ytimg.com/vi/{id}/{size}.jpg"), MAX_SOURCE).await?;
                if r.status == 200 && !r.body.is_empty() {
                    return Ok(r.body);
                }
            }
            Err(AppError::bad("YouTube has no thumbnail for that video (is it public?)"))
        }
        Video::Vimeo(id) => {
            let api = format!("https://vimeo.com/api/oembed.json?url={}&width=1280", urlencode(&format!("https://vimeo.com/{id}")));
            let r = get(&api, 64 * 1024).await?;
            if r.status != 200 {
                return Err(AppError::bad("Vimeo has no thumbnail for that video (is it public, and allowed to be embedded?)"));
            }
            let v: Value = serde_json::from_slice(&r.body).map_err(|_| AppError::bad("Vimeo sent something unexpected"))?;
            let thumb = v["thumbnail_url"].as_str().unwrap_or("");
            let host = url::Url::parse(thumb).ok().and_then(|u| u.host_str().map(str::to_ascii_lowercase)).unwrap_or_default();
            if !thumb.starts_with("https://") || !(host == "vimeocdn.com" || host.ends_with(".vimeocdn.com")) {
                return Err(AppError::bad("Vimeo has no thumbnail for that video"));
            }
            let r = get(thumb, MAX_SOURCE).await?;
            if r.status != 200 {
                return Err(AppError::bad("Vimeo's thumbnail couldn't be loaded"));
            }
            Ok(r.body)
        }
    }
}

fn urlencode(s: &str) -> String {
    url::form_urlencoded::byte_serialize(s.as_bytes()).collect()
}

fn decode(bytes: &[u8]) -> AppResult<DynamicImage> {
    let fmt = match crate::api::images::sniff(bytes) {
        Some("image/png") => ImageFormat::Png,
        Some("image/jpeg") => ImageFormat::Jpeg,
        Some("image/gif") => ImageFormat::Gif,
        Some("image/webp") => ImageFormat::WebP,
        _ => return Err(AppError::bad("that isn't a PNG, JPEG, GIF or WebP image")),
    };
    // Read the size from the header first: a small file can hold a huge picture.
    let (w, h) = image::ImageReader::with_format(std::io::Cursor::new(bytes), fmt).into_dimensions().map_err(|_| AppError::bad("that image couldn't be read"))?;
    if w > 8000 || h > 8000 || u64::from(w) * u64::from(h) > MAX_PIXELS {
        return Err(AppError::bad("that image is too big (at most 24 megapixels and 8000 px on a side)"));
    }
    let mut r = image::ImageReader::with_format(std::io::Cursor::new(bytes), fmt);
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(8000);
    limits.max_image_height = Some(8000);
    limits.max_alloc = Some(128 * 1024 * 1024);
    r.limits(limits);
    r.decode().map_err(|_| AppError::bad("that image couldn't be read"))
}

/// The image at most [`MAX_OUTPUT`] px wide and tall with a round play button in the middle, as a JPEG.
pub fn with_play_button(bytes: &[u8]) -> AppResult<Vec<u8>> {
    let img = decode(bytes)?;
    // `resize` keeps the proportions and fits inside both bounds.
    let img = if img.width() > MAX_OUTPUT || img.height() > MAX_OUTPUT { img.resize(MAX_OUTPUT, MAX_OUTPUT, image::imageops::FilterType::Triangle) } else { img };
    let (w, h) = img.dimensions();
    if w < 40 || h < 40 {
        return Err(AppError::bad("that image is too small for a video thumbnail"));
    }
    let mut canvas: RgbaImage = img.to_rgba8();
    let r = (w.min(h) as f32 * 0.14).clamp(18.0, 72.0);
    let (cx, cy) = (w as f32 / 2.0, h as f32 / 2.0);
    // Triangle pointing right, centered a little right of the circle's middle so it looks centered.
    let tri = [(cx - r * 0.32, cy - r * 0.48), (cx - r * 0.32, cy + r * 0.48), (cx + r * 0.52, cy)];
    let inside_tri = |x: f32, y: f32| {
        let s = |a: (f32, f32), b: (f32, f32)| (b.0 - a.0) * (y - a.1) - (b.1 - a.1) * (x - a.0);
        let (d1, d2, d3) = (s(tri[0], tri[1]), s(tri[1], tri[2]), s(tri[2], tri[0]));
        !((d1 < 0.0 || d2 < 0.0 || d3 < 0.0) && (d1 > 0.0 || d2 > 0.0 || d3 > 0.0))
    };
    let x0 = (cx - r - 2.0).max(0.0) as u32;
    let y0 = (cy - r - 2.0).max(0.0) as u32;
    let x1 = ((cx + r + 2.0) as u32).min(w - 1);
    let y1 = ((cy + r + 2.0) as u32).min(h - 1);
    for y in y0..=y1 {
        for x in x0..=x1 {
            // 4×4 samples per pixel for smooth edges.
            let (mut circle, mut white) = (0u32, 0u32);
            for sy in 0..4 {
                for sx in 0..4 {
                    let px = x as f32 + (sx as f32 + 0.5) / 4.0;
                    let py = y as f32 + (sy as f32 + 0.5) / 4.0;
                    let d = ((px - cx).powi(2) + (py - cy).powi(2)).sqrt();
                    if d <= r {
                        circle += 1;
                        if inside_tri(px, py) {
                            white += 1;
                        }
                    }
                }
            }
            if circle == 0 {
                continue;
            }
            let p = canvas.get_pixel_mut(x, y);
            let dark = circle as f32 / 16.0 * 0.62;
            let light = white as f32 / 16.0;
            let blend = |c: u8| {
                let c = c as f32 * (1.0 - dark);
                (c * (1.0 - light) + 255.0 * light).round().clamp(0.0, 255.0) as u8
            };
            *p = Rgba([blend(p[0]), blend(p[1]), blend(p[2]), 255]);
        }
    }
    let mut out = Vec::new();
    let rgb = DynamicImage::ImageRgba8(canvas).to_rgb8();
    let mut enc = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 85);
    enc.encode_image(&rgb).map_err(|e| AppError::Internal(e.into()))?;
    Ok(out)
}

/// The account's own image at `/i/{id}` on this server, if `url` is one.
async fn own_image(state: &AppState, account_id: Uuid, url: &str) -> AppResult<Option<Vec<u8>>> {
    let prefix = format!("{}/i/", state.cfg.public_url());
    let Some(id) = url.strip_prefix(&prefix).and_then(|r| r.split(['?', '#']).next()).and_then(|r| r.parse::<Uuid>().ok()) else { return Ok(None) };
    let bytes: Option<Vec<u8>> = sqlx::query_scalar("SELECT bytes FROM images WHERE id = $1 AND account_id = $2").bind(id).bind(account_id).fetch_optional(&state.db).await?;
    bytes.map(Some).ok_or_else(|| AppError::not_found("image"))
}

/// Make a video thumbnail: from `image_url` when given (an uploaded image, or any public image),
/// otherwise from the YouTube or Vimeo video at `video_url`.
pub async fn thumbnail(state: &AppState, account_id: Uuid, video_url: Option<&str>, image_url: Option<&str>) -> AppResult<Value> {
    // One at a time per account, taken before fetching anything.
    let _slot = take_slot(account_id)?;
    let image_url = image_url.map(str::trim).filter(|u| !u.is_empty());
    let video_url = video_url.map(str::trim).filter(|u| !u.is_empty());
    let (source, provider) = match (image_url, video_url) {
        (Some(u), _) => {
            let bytes = match own_image(state, account_id, u).await? {
                Some(b) => b,
                None => {
                    let r = get(u, MAX_SOURCE).await?;
                    if r.status != 200 {
                        return Err(AppError::bad(format!("that image address answered with HTTP {}", r.status)));
                    }
                    r.body
                }
            };
            (bytes, "image")
        }
        (None, Some(v)) => {
            let video = parse(v).ok_or_else(|| AppError::bad("we can fetch thumbnails for YouTube and Vimeo links; for other videos, upload a thumbnail image"))?;
            let provider = match video {
                Video::YouTube(_) => "youtube",
                Video::Vimeo(_) => "vimeo",
            };
            (provider_thumbnail(&video).await?, provider)
        }
        (None, None) => return Err(AppError::bad("give video_url (YouTube or Vimeo) or image_url")),
    };
    let permit = tokio::time::timeout(Duration::from_secs(10), CONVERTING.clone().acquire_owned())
        .await
        .map_err(|_| AppError::Busy("the server is busy making other thumbnails; try again in a minute".into()))?
        .map_err(|e| AppError::Internal(e.into()))?;
    let jpeg = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        with_play_button(&source)
    })
    .await
    .map_err(|e| AppError::Internal(e.into()))??;
    let stored = crate::api::images::store(state, account_id, &jpeg).await?;
    Ok(json!({"thumbnail_url": stored["url"], "image": stored, "source": provider}))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_video_links() {
        assert_eq!(parse("https://www.youtube.com/watch?v=dQw4w9WgXcQ&t=1"), Some(Video::YouTube("dQw4w9WgXcQ".into())));
        assert_eq!(parse("https://youtu.be/dQw4w9WgXcQ"), Some(Video::YouTube("dQw4w9WgXcQ".into())));
        assert_eq!(parse("https://youtube.com/shorts/dQw4w9WgXcQ"), Some(Video::YouTube("dQw4w9WgXcQ".into())));
        assert_eq!(parse("https://vimeo.com/76979871"), Some(Video::Vimeo("76979871".into())));
        assert_eq!(parse("https://player.vimeo.com/video/76979871"), Some(Video::Vimeo("76979871".into())));
        assert_eq!(parse("https://youtube.com.evil.test/watch?v=dQw4w9WgXcQ"), None);
        assert_eq!(parse("https://www.youtube.com/watch?v=../../x"), None);
        assert_eq!(parse("javascript:alert(1)"), None);
    }

    fn crc32(data: &[u8]) -> u32 {
        let mut c = 0xffff_ffffu32;
        for b in data {
            c ^= u32::from(*b);
            for _ in 0..8 {
                c = if c & 1 == 1 { 0xedb8_8320 ^ (c >> 1) } else { c >> 1 };
            }
        }
        !c
    }

    fn png_of(w: u32, h: u32) -> Vec<u8> {
        let mut png = Vec::new();
        DynamicImage::ImageRgba8(RgbaImage::new(w, h)).write_to(&mut std::io::Cursor::new(&mut png), ImageFormat::Png).unwrap();
        png
    }

    #[test]
    fn huge_pictures_are_refused_before_decoding_and_tall_ones_are_bounded() {
        // A tiny file that claims 7000 x 7000 pixels: refused from its header.
        let mut png = png_of(1, 1);
        png[16..20].copy_from_slice(&7000u32.to_be_bytes());
        png[20..24].copy_from_slice(&7000u32.to_be_bytes());
        let crc = crc32(&png[12..29]);
        png[29..33].copy_from_slice(&crc.to_be_bytes());
        let e = with_play_button(&png).unwrap_err();
        assert!(e.public_message().contains("too big"), "{}", e.public_message());
        let out = with_play_button(&png_of(300, 3000)).unwrap();
        let back = image::load_from_memory(&out).unwrap();
        assert!(back.height() <= MAX_OUTPUT && back.width() <= MAX_OUTPUT, "{}x{}", back.width(), back.height());
    }

    #[test]
    fn draws_a_play_button() {
        let mut img = RgbaImage::new(640, 360);
        for p in img.pixels_mut() {
            *p = Rgba([200, 30, 30, 255]);
        }
        let mut png = Vec::new();
        DynamicImage::ImageRgba8(img).write_to(&mut std::io::Cursor::new(&mut png), ImageFormat::Png).unwrap();
        let out = with_play_button(&png).unwrap();
        let back = image::load_from_memory(&out).unwrap().to_rgb8();
        let center = back.get_pixel(325, 180);
        assert!(center[0] > 230 && center[1] > 230, "white triangle in the middle: {center:?}");
        let ring = back.get_pixel(320, 180 - 45);
        assert!(ring[0] < 120, "darkened circle: {ring:?}");
        let corner = back.get_pixel(5, 5);
        assert!(corner[0] > 180 && corner[1] < 60, "untouched corner: {corner:?}");
    }
}
