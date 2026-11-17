//! Countdown timers: an animated GIF drawn on the server each time an email is opened, counting
//! down 60 seconds from the moment it's fetched (then holding), and a "this offer has ended" image
//! after the end time.
//!
//! The image address carries the end time and colors, signed with the server's secret, so nobody
//! can make DumbMail draw arbitrary images. Cost is bounded: a fixed size and frame count, one
//! drawing per timer per second (cached; every open in that second shares it), at most
//! [`MAX_DRAWING`] drawings at once per server process, and at most [`ACCOUNT_PER_MINUTE`] new
//! drawings a minute for one account (past that, opens get the last drawing).

use std::collections::HashMap;
use std::sync::Arc;

use ab_glyph::{Font, FontRef, PxScale, ScaleFont};
use axum::extract::{Path, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use base64::Engine;
use chrono::Utc;
use once_cell::sync::Lazy;
use serde_json::Value;
use tokio::sync::{Mutex, Semaphore};
use uuid::Uuid;

use super::prepare::{parse_time, Prep};
use crate::state::AppState;

const FONT: &[u8] = include_bytes!("assets/NotoSans-Bold-subset.ttf");
pub const WIDTH: u32 = 1104;
pub const HEIGHT: u32 = 240;
const FRAMES: i64 = 60;
pub const MAX_DRAWING: usize = 4;
pub const ACCOUNT_PER_MINUTE: u32 = 240;
const LABELS: [&str; 4] = ["DAYS", "HOURS", "MINUTES", "SECONDS"];
const DEFAULT_ENDED: &str = "This offer has ended";

/// What a timer image shows, read from its signed address.
#[derive(Debug, Clone, PartialEq)]
pub struct Timer {
    pub account_id: Uuid,
    pub ends_at: i64,
    pub background: [u8; 3],
    pub color: [u8; 3],
    pub label_color: [u8; 3],
    pub ended_text: String,
}

fn hex3(raw: &str) -> Option<[u8; 3]> {
    let h = raw.trim().trim_start_matches('#');
    let h = match h.len() {
        3 => h.chars().flat_map(|c| [c, c]).collect::<String>(),
        6 => h.to_string(),
        _ => return None,
    };
    let v = u32::from_str_radix(&h, 16).ok()?;
    Some([(v >> 16) as u8, (v >> 8) as u8, v as u8])
}

fn hex_of(c: [u8; 3]) -> String {
    format!("{:02x}{:02x}{:02x}", c[0], c[1], c[2])
}

/// Only characters the embedded font has; at most 40.
fn printable(s: &str) -> String {
    s.chars().filter(|c| (' '..='~').contains(c)).take(40).collect::<String>().trim().to_string()
}

impl Timer {
    fn payload(&self) -> String {
        let text = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(self.ended_text.as_bytes());
        format!("v1~{}~{}~{}~{}~{}~{}", self.account_id.simple(), self.ends_at, hex_of(self.background), hex_of(self.color), hex_of(self.label_color), text)
    }

    pub fn token(&self, secret: &str) -> String {
        crate::auth::sign(secret, &self.payload())
    }

    pub fn from_token(secret: &str, token: &str) -> Option<Self> {
        let payload = crate::auth::verify(secret, token)?;
        let p: Vec<&str> = payload.split('~').collect();
        if p.len() != 7 || p[0] != "v1" {
            return None;
        }
        let text = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(p[6]).ok()?;
        Some(Timer {
            account_id: Uuid::parse_str(p[1]).ok()?,
            ends_at: p[2].parse().ok()?,
            background: hex3(p[3])?,
            color: hex3(p[4])?,
            label_color: hex3(p[5])?,
            ended_text: printable(&String::from_utf8_lossy(&text)),
        })
    }
}

/// The image address and alt text for a countdown block, or None without a valid end time.
pub fn image_for(prep: &Prep<'_>, b: &Value) -> Option<(String, String)> {
    let s = |k: &str| b.get(k).and_then(|v| v.as_str()).unwrap_or("");
    let ends = parse_time(s("ends_at"))?;
    let ended = printable(s("ended_text"));
    let timer = Timer {
        account_id: prep.account.id,
        ends_at: ends.timestamp(),
        background: hex3(s("background")).unwrap_or([0x11, 0x18, 0x27]),
        color: hex3(s("color")).unwrap_or([0xff, 0xff, 0xff]),
        label_color: hex3(s("label_color")).unwrap_or([0x9c, 0xa3, 0xaf]),
        ended_text: if ended.is_empty() { DEFAULT_ENDED.into() } else { ended },
    };
    let tz = crate::sending::account_tz(prep.account);
    let local = ends.with_timezone(&tz);
    let alt = format!("Ends {}", local.format("%b %-d, %Y at %-I:%M %p %Z"));
    Some((format!("{}/e/countdown/{}.gif", prep.public_url, timer.token(prep.secret)), alt))
}

// ---------- Drawing ----------

/// A palette-indexed canvas: indices 0-15 blend background→color, 16-31 background→label color.
struct Canvas {
    px: Vec<u8>,
}

impl Canvas {
    fn new() -> Self {
        Canvas { px: vec![0; (WIDTH * HEIGHT) as usize] }
    }

    fn text_width(font: &FontRef<'_>, size: f32, text: &str) -> f32 {
        let sf = font.as_scaled(PxScale::from(size));
        let mut w = 0.0;
        let mut prev = None;
        for c in text.chars() {
            let id = sf.glyph_id(c);
            if let Some(p) = prev {
                w += sf.kern(p, id);
            }
            w += sf.h_advance(id);
            prev = Some(id);
        }
        w
    }

    /// Draw `text` centered on `cx` with its baseline at `y`, in palette ramp `base` (0 or 16).
    fn text(&mut self, font: &FontRef<'_>, size: f32, text: &str, cx: f32, y: f32, base: u8) {
        let sf = font.as_scaled(PxScale::from(size));
        let mut x = cx - Self::text_width(font, size, text) / 2.0;
        let mut prev = None;
        for c in text.chars() {
            let id = sf.glyph_id(c);
            if let Some(p) = prev {
                x += sf.kern(p, id);
            }
            let glyph = id.with_scale_and_position(PxScale::from(size), ab_glyph::point(x, y));
            if let Some(outline) = font.outline_glyph(glyph) {
                let bounds = outline.px_bounds();
                outline.draw(|gx, gy, cov| {
                    let px = bounds.min.x as i32 + gx as i32;
                    let py = bounds.min.y as i32 + gy as i32;
                    if px < 0 || py < 0 || px >= WIDTH as i32 || py >= HEIGHT as i32 {
                        return;
                    }
                    let level = (cov.clamp(0.0, 1.0) * 15.0).round() as u8;
                    let i = (py as u32 * WIDTH + px as u32) as usize;
                    let cur = self.px[i];
                    let cur_level = if cur >= 16 { cur - 16 } else { cur };
                    if level > cur_level || cur == 0 {
                        if level > 0 {
                            self.px[i] = base + level;
                        }
                    }
                });
            }
            x += sf.h_advance(id);
            prev = Some(id);
        }
    }
}

fn palette(t: &Timer) -> Vec<u8> {
    let mut p = Vec::with_capacity(32 * 3);
    for target in [t.color, t.label_color] {
        for i in 0..16u32 {
            for c in 0..3 {
                let v = (t.background[c] as u32 * (15 - i) + target[c] as u32 * i) / 15;
                p.push(v as u8);
            }
        }
    }
    p
}

fn units(left: i64) -> [i64; 4] {
    let left = left.max(0);
    [(left / 86400).min(999), (left % 86400) / 3600, (left % 3600) / 60, left % 60]
}

fn frame(font: &FontRef<'_>, t: &Timer, left: i64) -> Canvas {
    let mut c = Canvas::new();
    if left <= 0 {
        c.text(font, 64.0, &t.ended_text, WIDTH as f32 / 2.0, HEIGHT as f32 / 2.0 + 22.0, 0);
        return c;
    }
    let cell = WIDTH as f32 / 4.0;
    for (i, v) in units(left).iter().enumerate() {
        let cx = cell * i as f32 + cell / 2.0;
        c.text(font, 112.0, &format!("{v:02}"), cx, 140.0, 0);
        c.text(font, 28.0, LABELS[i], cx, 196.0, 16);
        if i > 0 {
            c.text(font, 72.0, ":", cell * i as f32, 124.0, 16);
        }
    }
    c
}

/// The GIF for a timer as seen at unix time `now`: up to 60 one-second frames, played once.
pub fn draw(t: &Timer, now: i64) -> Vec<u8> {
    let font = FontRef::try_from_slice(FONT).expect("embedded font");
    let pal = palette(t);
    let mut out = Vec::new();
    {
        let mut enc = gif::Encoder::new(&mut out, WIDTH as u16, HEIGHT as u16, &pal).expect("gif header");
        let left = t.ends_at - now;
        let n = if left <= 0 { 1 } else { FRAMES.min(left + 1) };
        let mut prev: Option<Canvas> = None;
        for k in 0..n {
            let cur = frame(&font, t, left - k);
            // After the first frame, only the area that changed is stored.
            let (x0, y0, x1, y1) = match &prev {
                None => (0, 0, WIDTH - 1, HEIGHT - 1),
                Some(p) => {
                    let (mut x0, mut y0, mut x1, mut y1) = (WIDTH, HEIGHT, 0, 0);
                    for y in 0..HEIGHT {
                        for x in 0..WIDTH {
                            let i = (y * WIDTH + x) as usize;
                            if p.px[i] != cur.px[i] {
                                x0 = x0.min(x);
                                y0 = y0.min(y);
                                x1 = x1.max(x);
                                y1 = y1.max(y);
                            }
                        }
                    }
                    if x0 > x1 { (0, 0, 0, 0) } else { (x0, y0, x1, y1) }
                }
            };
            let (w, h) = (x1 - x0 + 1, y1 - y0 + 1);
            let mut buf = Vec::with_capacity((w * h) as usize);
            for y in y0..=y1 {
                let row = (y * WIDTH) as usize;
                buf.extend_from_slice(&cur.px[row + x0 as usize..=row + x1 as usize]);
            }
            let mut f = gif::Frame::default();
            f.left = x0 as u16;
            f.top = y0 as u16;
            f.width = w as u16;
            f.height = h as u16;
            f.delay = 100;
            f.buffer = std::borrow::Cow::Owned(buf);
            enc.write_frame(&f).expect("gif frame");
            prev = Some(cur);
        }
    }
    out
}

// ---------- Serving ----------

struct Cached {
    second: i64,
    gif: Arc<Vec<u8>>,
}

static CACHE: Lazy<Mutex<HashMap<String, Cached>>> = Lazy::new(|| Mutex::new(HashMap::new()));
static BUDGET: Lazy<Mutex<HashMap<Uuid, (i64, u32)>>> = Lazy::new(|| Mutex::new(HashMap::new()));
static DRAWING: Lazy<Semaphore> = Lazy::new(|| Semaphore::new(MAX_DRAWING));

/// Count one drawing against the account's per-minute budget; false when it's used up.
async fn take_budget(account: Uuid, now: i64) -> bool {
    let mut b = BUDGET.lock().await;
    if b.len() > 10_000 {
        b.retain(|_, (minute, _)| *minute == now / 60);
    }
    let e = b.entry(account).or_insert((now / 60, 0));
    if e.0 != now / 60 {
        *e = (now / 60, 0);
    }
    if e.1 >= ACCOUNT_PER_MINUTE {
        return false;
    }
    e.1 += 1;
    true
}

fn gif_response(bytes: Arc<Vec<u8>>) -> Response {
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "image/gif"),
            // Every open should see the time left at that moment.
            (header::CACHE_CONTROL, "no-cache, no-store, must-revalidate, max-age=0"),
        ],
        bytes.as_ref().clone(),
    )
        .into_response()
}

/// The image for `token` at `now`, drawing it if no open in this second already did.
pub async fn image(secret: &str, token: &str, now: i64) -> Result<Arc<Vec<u8>>, StatusCode> {
    let t = Timer::from_token(secret, token).ok_or(StatusCode::NOT_FOUND)?;
    let ended = t.ends_at <= now;
    {
        let cache = CACHE.lock().await;
        if let Some(c) = cache.get(token) {
            // An ended timer never changes again.
            if c.second == now || (ended && c.second >= t.ends_at) {
                return Ok(c.gif.clone());
            }
        }
    }
    if !take_budget(t.account_id, now).await {
        let cache = CACHE.lock().await;
        return cache.get(token).map(|c| c.gif.clone()).ok_or(StatusCode::TOO_MANY_REQUESTS);
    }
    let _permit = DRAWING.acquire().await.map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;
    let gif = Arc::new(tokio::task::spawn_blocking(move || draw(&t, now)).await.map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?);
    let mut cache = CACHE.lock().await;
    if cache.len() > 2_000 {
        cache.retain(|_, c| c.second >= now - 60);
        if cache.len() > 2_000 {
            cache.clear();
        }
    }
    cache.insert(token.to_string(), Cached { second: now, gif: gif.clone() });
    Ok(gif)
}

pub async fn image_h(State(state): State<AppState>, Path(file): Path<String>) -> Response {
    let Some(token) = file.strip_suffix(".gif") else { return StatusCode::NOT_FOUND.into_response() };
    match image(&state.cfg.secret_key, token, Utc::now().timestamp()).await {
        Ok(gif) => gif_response(gif),
        Err(code) => code.into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn timer() -> Timer {
        Timer { account_id: Uuid::nil(), ends_at: 1_000_000, background: [17, 24, 39], color: [255, 255, 255], label_color: [156, 163, 175], ended_text: "Sale over".into() }
    }

    #[test]
    fn tokens_round_trip_and_reject_tampering() {
        let t = timer();
        let tok = t.token("s");
        assert_eq!(Timer::from_token("s", &tok), Some(t.clone()));
        assert!(Timer::from_token("other", &tok).is_none());
        let forged = tok.replacen("1000000", "2000000", 1);
        assert!(Timer::from_token("s", &forged).is_none());
    }

    #[test]
    fn draws_a_countdown_and_an_ended_image() {
        let t = timer();
        let running = draw(&t, t.ends_at - 90_061);
        assert!(running.starts_with(b"GIF89a"));
        let mut d = gif::DecodeOptions::new();
        d.set_color_output(gif::ColorOutput::Indexed);
        let mut r = d.clone().read_info(running.as_slice()).unwrap();
        let mut frames = 0;
        while r.read_next_frame().unwrap().is_some() {
            frames += 1;
        }
        assert_eq!(frames, 60);
        assert!(running.len() < 200_000, "{} bytes", running.len());
        // Ten seconds before the end: ten frames down to zero, then the ended frame.
        let ending = draw(&t, t.ends_at - 10);
        let mut r = d.clone().read_info(ending.as_slice()).unwrap();
        let mut frames = 0;
        while r.read_next_frame().unwrap().is_some() {
            frames += 1;
        }
        assert_eq!(frames, 11);
        let ended = draw(&t, t.ends_at + 5);
        let mut r = gif::DecodeOptions::new().read_info(ended.as_slice()).unwrap();
        assert!(r.read_next_frame().unwrap().is_some());
        assert!(r.read_next_frame().unwrap().is_none());
    }

    #[test]
    fn units_split_and_cap() {
        assert_eq!(units(90_061), [1, 1, 1, 1]);
        assert_eq!(units(-5), [0, 0, 0, 0]);
        assert_eq!(units(10_000 * 86_400)[0], 999);
    }
}
