//! Checks before sending: problems a sender can fix, each saying what to do. Only the problems the
//! product already refuses to send with (from `campaigns::readiness`, an empty audience, content
//! that doesn't render) are marked `blocks_sending`; everything else is advice.
//!
//! One check runs at a time per account, and everything a check does against the database (counting
//! the audience, trying display rules on it) runs behind that, at most [`GLOBAL_DB_CHECKS`] across
//! the server and within [`AUDIENCE_DEADLINE`].
//!
//! Link and image checks request addresses found in the email through [`super::net`]: public
//! addresses only, no redirects followed, at most [`MAX_URLS`] addresses, [`PARALLEL`] at a time per
//! check and [`GLOBAL_PARALLEL`] across the server, a deadline on the whole check, and results cached
//! for [`CACHE_SECS`] seconds. Addresses on DumbMail's own hosts (the app, the marketing site, the
//! shared link host, retired hosts and every account's branded link domain) are never requested: a
//! copied tracking link or open pixel would count as a real click or open. A tracked link is checked
//! as the address it leads to.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use once_cell::sync::Lazy;
use regex::Regex;
use reqwest::Method;
use serde::Deserialize;
use serde_json::{json, Value};
use sqlx::{Postgres, QueryBuilder};
use tokio::sync::{Mutex, Semaphore};
use uuid::Uuid;

use super::net::{self, NetError};
use crate::campaigns::{self, Audience};
use crate::error::{AppError, AppResult};
use crate::models::Account;
use crate::state::AppState;

pub const MAX_URLS: usize = 40;
pub const PARALLEL: usize = 6;
pub const GLOBAL_PARALLEL: usize = 24;
pub const CACHE_SECS: u64 = 600;
pub const GLOBAL_DB_CHECKS: usize = 4;
/// The most people display rules are tried on.
pub const RULE_SAMPLE: i64 = 5_000;
pub const AUDIENCE_DEADLINE: Duration = Duration::from_secs(15);
const DEADLINE: Duration = Duration::from_secs(20);
const PER_URL: Duration = Duration::from_secs(6);
pub const GMAIL_CLIP_BYTES: usize = 102 * 1024;
pub const HEAVY_IMAGE_BYTES: u64 = 1024 * 1024;
const SHORTENERS: [&str; 14] = ["bit.ly", "tinyurl.com", "t.co", "goo.gl", "ow.ly", "is.gd", "buff.ly", "rebrand.ly", "cutt.ly", "shorturl.at", "tiny.cc", "rb.gy", "t.ly", "bl.ink"];

#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct Input {
    /// The email to check; defaults to the campaign's (version A) or the flow step's saved content.
    pub content: Option<Value>,
    pub campaign_id: Option<Uuid>,
    /// Who it goes to, when it differs from the saved campaign's (an unsaved edit).
    pub audience: Option<Value>,
    pub flow_id: Option<Uuid>,
    pub node_id: Option<String>,
    /// Skip requesting links and images (faster; no network).
    #[serde(default)]
    pub skip_links: bool,
}

#[derive(Debug, Clone)]
pub struct Finding {
    pub id: &'static str,
    pub severity: &'static str,
    pub title: String,
    pub detail: String,
    pub blocks_sending: bool,
}

fn finding(id: &'static str, severity: &'static str, title: impl Into<String>, detail: impl Into<String>) -> Finding {
    Finding { id, severity, title: title.into(), detail: detail.into(), blocks_sending: false }
}

fn blocking(id: &'static str, title: impl Into<String>, detail: impl Into<String>) -> Finding {
    Finding { blocks_sending: true, ..finding(id, "error", title, detail) }
}

fn s<'a>(b: &'a Value, k: &str) -> &'a str {
    b.get(k).and_then(|v| v.as_str()).unwrap_or("")
}

// ---------- What's in the email ----------

#[derive(Debug, Default)]
pub struct Scan {
    /// (address, link text if known)
    pub links: Vec<(String, String)>,
    /// (src, alt, linked)
    pub images: Vec<(String, String, bool)>,
    /// Buttons and links with nothing to go to.
    pub empty_links: usize,
    /// Links with a scheme email apps refuse (javascript:, data:).
    pub bad_links: Vec<String>,
    pub text_chars: usize,
    pub strings: Vec<String>,
}

static ANCHOR: Lazy<Regex> = Lazy::new(|| Regex::new(r#"(?is)<a\s[^>]*?href\s*=\s*["']([^"']*)["'][^>]*>(.*?)</a>"#).unwrap());
static IMG: Lazy<Regex> = Lazy::new(|| Regex::new(r#"(?is)<img\s[^>]*>"#).unwrap());
static ATTR_SRC: Lazy<Regex> = Lazy::new(|| Regex::new(r#"(?is)\ssrc\s*=\s*["']([^"']*)["']"#).unwrap());
static ATTR_ALT: Lazy<Regex> = Lazy::new(|| Regex::new(r#"(?is)\salt\s*=\s*["']([^"']*)["']"#).unwrap());
static TAGS: Lazy<Regex> = Lazy::new(|| Regex::new(r"<[^>]+>").unwrap());

impl Scan {
    fn link(&mut self, href: &str, text: &str) {
        let h = href.trim();
        let lower = h.to_ascii_lowercase();
        if h.is_empty() || h == "#" || lower == "https://" || lower == "http://" {
            self.empty_links += 1;
        } else if lower.starts_with("javascript:") || lower.starts_with("data:") || lower.starts_with("vbscript:") {
            self.bad_links.push(h.to_string());
        } else {
            self.links.push((h.to_string(), text.trim().to_string()));
        }
    }

    fn html(&mut self, html: &str) {
        for c in ANCHOR.captures_iter(html) {
            let text = TAGS.replace_all(&c[2], "").trim().to_string();
            // The address as a browser reads the attribute.
            self.link(&c[1].replace("&amp;", "&"), &text);
        }
        for tag in IMG.find_iter(html) {
            let src = ATTR_SRC.captures(tag.as_str()).map(|c| c[1].to_string()).unwrap_or_default();
            let alt = ATTR_ALT.captures(tag.as_str()).map(|c| c[1].to_string()).unwrap_or_default();
            if !src.is_empty() {
                self.images.push((src, alt, false));
            }
        }
        self.text_chars += TAGS.replace_all(html, " ").split_whitespace().map(|w| w.chars().count()).sum::<usize>();
    }

    fn image(&mut self, src: &str, alt: &str, href: &str) {
        if !src.trim().is_empty() {
            self.images.push((src.trim().to_string(), alt.trim().to_string(), !href.trim().is_empty()));
        }
        if !href.trim().is_empty() {
            self.link(href, alt);
        }
    }

    fn words(&mut self, t: &str) {
        self.text_chars += t.split_whitespace().map(|w| w.chars().count()).sum::<usize>();
    }

    fn block(&mut self, b: &Value) {
        let collect = |v: &Value, out: &mut Vec<String>| collect_strings(v, out);
        collect(b, &mut self.strings);
        match s(b, "type") {
            "heading" => self.words(s(b, "text")),
            "text" | "html" => self.html(s(b, "html")),
            "button" => {
                self.words(s(b, "text"));
                self.link(s(b, "href"), s(b, "text"));
            }
            "image" => self.image(s(b, "src"), s(b, "alt"), s(b, "href")),
            "product" => {
                self.words(s(b, "title"));
                self.image(s(b, "image"), s(b, "title"), "");
                self.link(s(b, "href"), s(b, "title"));
            }
            "video" => {
                self.image(s(b, "thumbnail"), s(b, "alt"), "");
                self.link(s(b, "url"), s(b, "alt"));
            }
            "image_text" => {
                self.image(s(b, "src"), s(b, "alt"), s(b, "href"));
                self.words(s(b, "heading"));
                self.html(s(b, "html"));
                if !s(b, "button_text").is_empty() {
                    self.link(s(b, "button_href"), s(b, "button_text"));
                }
            }
            "quote" => {
                self.words(s(b, "text"));
                self.image(s(b, "image"), s(b, "author"), "");
            }
            "header" | "nav" | "footer" => {
                if s(b, "type") == "header" {
                    self.image(s(b, "logo"), s(b, "logo_alt"), s(b, "logo_href"));
                }
                self.words(s(b, "text"));
                for l in b.get("links").and_then(|l| l.as_array()).into_iter().flatten() {
                    self.words(s(l, "text"));
                    self.link(s(l, "href"), s(l, "text"));
                }
                for i in b.get("_icons").and_then(|l| l.as_array()).into_iter().flatten() {
                    self.link(s(i, "href"), "");
                }
            }
            "social" => {
                for i in b.get("_icons").and_then(|l| l.as_array()).into_iter().flatten() {
                    self.link(s(i, "href"), "");
                }
            }
            "table" => {
                for r in b.get("rows").and_then(|r| r.as_array()).into_iter().flatten() {
                    for c in r.as_array().into_iter().flatten() {
                        self.words(c.as_str().unwrap_or(""));
                    }
                }
                if b.get("loop").is_some() {
                    self.text_chars += 40;
                }
            }
            "columns" => {
                for col in b.get("columns").and_then(|c| c.as_array()).into_iter().flatten() {
                    for x in col.as_array().into_iter().flatten() {
                        self.block(x);
                    }
                }
            }
            "products" => self.text_chars += 40,
            _ => {}
        }
    }
}

fn collect_strings(v: &Value, out: &mut Vec<String>) {
    match v {
        Value::String(s) if s.contains("{{") || s.contains("{%") => out.push(s.clone()),
        Value::Array(a) => a.iter().for_each(|x| collect_strings(x, out)),
        Value::Object(o) => o.iter().filter(|(k, _)| !k.starts_with('_')).for_each(|(_, x)| collect_strings(x, out)),
        _ => {}
    }
}

pub fn scan(content: &Value) -> Scan {
    let mut sc = Scan::default();
    match content.get("blocks").and_then(|b| b.as_array()).filter(|b| !b.is_empty()) {
        Some(blocks) => blocks.iter().for_each(|b| sc.block(b)),
        None => {
            let html = s(content, "html");
            sc.html(html);
            collect_strings(&json!(html), &mut sc.strings);
        }
    }
    for k in ["subject", "preheader"] {
        collect_strings(&content[k], &mut sc.strings);
    }
    sc
}

// ---------- Personalization ----------

/// A personalization variable with no fallback, as the profile field it reads.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Var {
    Field(&'static str),
    Property(String),
    Event(String),
}

impl Var {
    pub fn tag(&self) -> String {
        match self {
            Var::Field(f) => (*f).to_string(),
            Var::Property(p) => format!("person.properties.{p}"),
            Var::Event(e) => e.clone(),
        }
    }
}

static EXPR: Lazy<Regex> = Lazy::new(|| Regex::new(r"\{\{\s*(.*?)\s*\}\}").unwrap());
static LOOP_VAR: Lazy<Regex> = Lazy::new(|| Regex::new(r"\{%-?\s*for\s+([A-Za-z_]\w*)(?:\s*,\s*([A-Za-z_]\w*))?\s+in\b").unwrap());
static PATH: Lazy<Regex> = Lazy::new(|| Regex::new(r"^([A-Za-z_]\w*(?:\.[A-Za-z_]\w*)*)").unwrap());

/// Variables used without a `default(...)`, which come out empty for people missing them.
pub fn bare_vars(strings: &[String]) -> Vec<Var> {
    let mut loop_vars: HashSet<String> = ["loop", "item"].into_iter().map(String::from).collect();
    for st in strings {
        for c in LOOP_VAR.captures_iter(st) {
            loop_vars.insert(c[1].to_string());
            if let Some(m) = c.get(2) {
                loop_vars.insert(m.as_str().to_string());
            }
        }
    }
    let mut out: Vec<Var> = Vec::new();
    for st in strings {
        for c in EXPR.captures_iter(st) {
            let e = c[1].trim();
            if e.contains("default(") || e.contains("|d(") || e.contains(" or ") || e.contains(" if ") {
                continue;
            }
            let Some(path) = PATH.captures(e).map(|p| p[1].to_string()) else { continue };
            let parts: Vec<&str> = path.split('.').collect();
            let var = match parts.as_slice() {
                [r, ..] if loop_vars.contains(*r) => None,
                ["organization" | "unsubscribe_url" | "catalog" | "recommendations" | "range" | "true" | "false" | "none", ..] => None,
                ["first_name"] | ["person", "first_name"] => Some(Var::Field("first_name")),
                ["last_name"] | ["person", "last_name"] => Some(Var::Field("last_name")),
                ["email"] | ["person", "email"] => Some(Var::Field("email")),
                ["phone_number"] | ["person", "phone_number"] => Some(Var::Field("phone_number")),
                ["person", "properties", key, ..] => Some(Var::Property(key.to_string())),
                ["person", ..] => None,
                ["event", ..] => Some(Var::Event(path.clone())),
                [key, ..] => Some(Var::Property(key.to_string())),
                [] => None,
            };
            if let Some(v) = var {
                if !out.contains(&v) {
                    out.push(v);
                }
            }
        }
    }
    out.truncate(20);
    out
}

fn push_empty(qb: &mut QueryBuilder<'_, Postgres>, v: &Var) {
    match v {
        Var::Field(f) => {
            // The field name comes from a fixed list, never from the email.
            qb.push(format!("NULLIF(trim(p.{f}), '') IS NULL"));
        }
        Var::Property(k) => {
            qb.push("COALESCE(p.properties ->> ").push_bind(k.clone()).push(", '') = ''");
        }
        Var::Event(_) => {
            qb.push("false");
        }
    }
}

/// Recipient counts: (reachable, never subscribed, missing each variable). With no audience,
/// everyone on the account who can get email.
async fn counts(state: &AppState, account_id: Uuid, audience: Option<&Audience>, vars: &[Var]) -> AppResult<(i64, i64, Vec<i64>)> {
    let mut qb = QueryBuilder::<Postgres>::new("SELECT count(*), count(*) FILTER (WHERE p.email_consent = 'never_subscribed')");
    for v in vars {
        qb.push(", count(*) FILTER (WHERE ");
        push_empty(&mut qb, v);
        qb.push(")");
    }
    match audience {
        Some(a) => campaigns::append_audience(&mut qb, &state.db, account_id, "email", a).await?,
        None => {
            qb.push(" FROM profiles p WHERE p.account_id = ").push_bind(account_id);
            campaigns::push_eligible(&mut qb, "email");
        }
    }
    let mut tx = state.db.begin().await?;
    // A check is advice: never let it run long against the database live sending uses.
    sqlx::query("SET LOCAL statement_timeout = '10s'").execute(&mut *tx).await?;
    let row = qb.build().fetch_one(&mut *tx).await?;
    tx.commit().await?;
    use sqlx::Row;
    let total: i64 = row.try_get(0)?;
    let never: i64 = row.try_get(1)?;
    let missing = (0..vars.len()).map(|i| row.try_get::<i64, _>(i + 2)).collect::<Result<Vec<_>, _>>()?;
    Ok((total, never, missing))
}

// ---------- Links and images ----------

#[derive(Debug, Clone)]
pub struct Probe {
    pub ok: Option<bool>,
    pub status: Option<u16>,
    pub note: String,
    pub bytes: Option<u64>,
}

/// What a link's answer means for the person clicking it. `ok` None: couldn't tell.
pub fn judge(status: u16) -> (Option<bool>, &'static str) {
    match status {
        200..=299 => (Some(true), "works"),
        300..=399 => (None, "redirects elsewhere (not followed, so not checked)"),
        401 | 403 | 429 => (None, "refused our check (it may still work in a browser)"),
        404 | 410 => (Some(false), "page not found"),
        400..=499 => (Some(false), "the site refused the request"),
        _ => (Some(false), "the site had an error"),
    }
}

static CACHE: Lazy<Mutex<HashMap<(Uuid, String), (Instant, Probe)>>> = Lazy::new(|| Mutex::new(HashMap::new()));
static GLOBAL: Lazy<Arc<Semaphore>> = Lazy::new(|| Arc::new(Semaphore::new(GLOBAL_PARALLEL)));
// A plain (not async) lock: it is held only to insert or remove an id, never across an await, so
// a finished check can give its slot back at once when it's dropped.
static RUNNING: Lazy<std::sync::Mutex<HashSet<Uuid>>> = Lazy::new(|| std::sync::Mutex::new(HashSet::new()));
static DB_CHECKS: Lazy<Arc<Semaphore>> = Lazy::new(|| Arc::new(Semaphore::new(GLOBAL_DB_CHECKS)));

/// What an answer means: a redirect isn't verified (it isn't followed), and an image address that
/// answers with a web page (a login wall, an error page) shows a broken image.
pub fn assess(r: &net::Response, image: bool) -> Probe {
    let (ok, note) = judge(r.status);
    let success = (200..300).contains(&r.status);
    let bytes = r.content_length.or_else(|| (image && success).then_some(r.body.len() as u64));
    let is_image = r.content_type.starts_with("image/") || (r.content_type.is_empty() && crate::api::images::sniff(&r.body).is_some());
    if image && success && !is_image {
        return Probe { ok: Some(false), status: Some(r.status), note: "isn't an image (the address answers with a web page or another kind of file)".into(), bytes };
    }
    Probe { ok, status: Some(r.status), note: note.to_string(), bytes }
}

async fn probe(url: &str, image: bool) -> Probe {
    let failed = |e: NetError| Probe { ok: if matches!(e, NetError::NotPublic | NetError::Invalid | NetError::NoSuchDomain) { Some(false) } else { None }, status: None, note: e.describe().to_string(), bytes: None };
    let head = net::request(Method::HEAD, url, 0, PER_URL).await;
    let res = match head {
        Ok(r) if !matches!(r.status, 400 | 403 | 405 | 501) && (!image || !(200..300).contains(&r.status) || (r.content_length.is_some() && !r.content_type.is_empty())) => Ok(r),
        Err(e @ (NetError::NotPublic | NetError::Invalid | NetError::NoSuchDomain)) => Err(e),
        // Some servers don't answer HEAD properly; ask again with GET.
        _ => net::request(Method::GET, url, if image { 2 * 1024 * 1024 + 1 } else { 16 * 1024 }, PER_URL).await,
    };
    match res {
        Ok(r) => assess(&r, image),
        Err(e) => failed(e),
    }
}

/// Check each address (link or image), using recent results when there are any.
async fn probe_all(account_id: Uuid, urls: Vec<(String, bool)>) -> Vec<(String, bool, Probe)> {
    let now = Instant::now();
    let mut out = Vec::new();
    let mut todo = Vec::new();
    {
        let mut cache = CACHE.lock().await;
        cache.retain(|_, (at, _)| now.duration_since(*at) < Duration::from_secs(CACHE_SECS));
        for (u, img) in urls {
            match cache.get(&(account_id, u.clone())) {
                Some((_, p)) => out.push((u, img, p.clone())),
                None => todo.push((u, img)),
            }
        }
    }
    let local = Arc::new(Semaphore::new(PARALLEL));
    let mut set = tokio::task::JoinSet::new();
    for (u, img) in todo.iter().cloned() {
        let local = local.clone();
        set.spawn(async move {
            let _l = local.acquire_owned().await.ok();
            let _g = GLOBAL.clone().acquire_owned().await.ok();
            let p = probe(&u, img).await;
            (u, img, p)
        });
    }
    let deadline = tokio::time::sleep(DEADLINE);
    tokio::pin!(deadline);
    let mut done: Vec<(String, bool, Probe)> = Vec::new();
    loop {
        tokio::select! {
            r = set.join_next() => match r {
                Some(Ok(x)) => done.push(x),
                Some(Err(_)) => {}
                None => break,
            },
            _ = &mut deadline => {
                set.abort_all();
                break;
            }
        }
    }
    {
        let mut cache = CACHE.lock().await;
        if cache.len() > 5_000 {
            cache.clear();
        }
        for (u, _, p) in &done {
            if p.ok.is_some() || p.status.is_some() {
                cache.insert((account_id, u.clone()), (Instant::now(), p.clone()));
            }
        }
    }
    for (u, img) in todo {
        if !done.iter().any(|(d, _, _)| *d == u) {
            done.push((u, img, Probe { ok: None, status: None, note: "not checked: the check ran out of time".into(), bytes: None }));
        }
    }
    out.extend(done);
    out
}

/// Releases the account's one-check-at-a-time slot when dropped.
struct Slot(Uuid);

impl Drop for Slot {
    fn drop(&mut self) {
        // Released before the response goes out, so the next check right after this one isn't
        // told one is still running.
        RUNNING.lock().unwrap_or_else(|e| e.into_inner()).remove(&self.0);
    }
}

async fn take_slot(account_id: Uuid) -> AppResult<Slot> {
    let mut r = RUNNING.lock().unwrap_or_else(|e| e.into_inner());
    if !r.insert(account_id) {
        return Err(AppError::Busy("a check is already running for this account; try again in a few seconds".into()));
    }
    Ok(Slot(account_id))
}

/// Where an address found in the email points.
#[derive(Debug, PartialEq)]
pub enum Target {
    /// Somewhere else: check it.
    External(String),
    /// One of the account's images on a DumbMail host.
    OwnImage(Uuid),
    /// A DumbMail address (tracking, unsubscribe, forms, hosted images): not requested.
    Own,
}

fn exact_host(u: &str) -> Option<String> {
    url::Url::parse(u).ok().and_then(|u| u.host_str().map(|h| h.trim_end_matches('.').to_ascii_lowercase()))
}

/// Where a click-tracking link (`/t/c/...?u=`) leads, on any host.
fn tracked_destination(url: &str) -> Option<String> {
    let parsed = url::Url::parse(url).ok()?;
    if !parsed.path().starts_with("/t/c/") {
        return None;
    }
    parsed.query_pairs().find(|(k, _)| k == "u").map(|(_, v)| v.into_owned())
}

/// What `url` is, given DumbMail's own hosts. A click-tracking link is judged by where it leads.
pub fn classify(url: &str, own: &HashSet<String>) -> Target {
    let Some(host) = exact_host(url) else { return Target::External(url.to_string()) };
    if !own.contains(&host) {
        return Target::External(url.to_string());
    }
    let Ok(parsed) = url::Url::parse(url) else { return Target::Own };
    let path = parsed.path();
    if path.starts_with("/t/c/") {
        if let Some((_, dest)) = parsed.query_pairs().find(|(k, _)| k == "u") {
            if exact_host(&dest).is_some_and(|h| !own.contains(&h)) && (dest.starts_with("https://") || dest.starts_with("http://")) {
                return Target::External(dest.into_owned());
            }
        }
        return Target::Own;
    }
    match path.strip_prefix("/i/").and_then(|r| r.parse::<Uuid>().ok()) {
        Some(id) => Target::OwnImage(id),
        None => Target::Own,
    }
}

/// DumbMail's own hostnames among `hosts`: the configured ones, and any account's link domain.
async fn own_hosts(state: &AppState, hosts: &HashSet<String>) -> AppResult<HashSet<String>> {
    let cfg = &state.cfg;
    let mut own: HashSet<String> = [cfg.public_host(), cfg.site_host(), cfg.shared_link_host(), exact_host(cfg.link_url())].into_iter().flatten().collect();
    own.extend(cfg.redirect_hosts.iter().map(|h| h.trim().trim_end_matches('.').to_ascii_lowercase()).filter(|h| !h.is_empty()));
    if let Some(site) = cfg.site_host() {
        own.insert(format!("www.{site}"));
    }
    let unknown: Vec<String> = hosts.iter().filter(|h| !own.contains(*h)).cloned().collect();
    if !unknown.is_empty() {
        let found: Vec<String> = sqlx::query_scalar("SELECT DISTINCT lower(link_domain) FROM accounts WHERE lower(link_domain) = ANY($1)").bind(&unknown).fetch_all(&state.db).await?;
        own.extend(found);
    }
    Ok(own)
}

fn host_of(u: &str) -> Option<String> {
    url::Url::parse(u).ok().and_then(|u| u.host_str().map(|h| h.trim_start_matches("www.").to_ascii_lowercase()))
}

/// Link text that is itself a web address pointing somewhere other than the link.
pub fn mismatched(text: &str, href: &str) -> bool {
    static DOMAINISH: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)^(https?://)?(www\.)?([a-z0-9-]+\.)+[a-z]{2,}(/\S*)?$").unwrap());
    let t = text.trim();
    if !DOMAINISH.is_match(t) || href.contains("{{") {
        return false;
    }
    let shown = host_of(&if t.starts_with("http") { t.to_string() } else { format!("https://{t}") });
    match (shown, host_of(href)) {
        (Some(a), Some(b)) => !(a == b || b.ends_with(&format!(".{a}")) || a.ends_with(&format!(".{b}"))),
        _ => false,
    }
}

fn plural(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

// ---------- The check ----------

use super::prepare::sample_ctx;

/// Display rules tried on (up to [`RULE_SAMPLE`] of) the audience: (people tried, people every
/// block is hidden for, people a rule couldn't be decided for).
async fn rule_counts(state: &AppState, account: &Account, audience: Option<&Audience>, content: &Value, saved: &super::prepare::SavedMap) -> AppResult<(i64, i64, i64)> {
    let mut qb = QueryBuilder::<Postgres>::new(format!("SELECT {}", crate::models::PROFILE_COLS));
    match audience {
        Some(a) => campaigns::append_audience(&mut qb, &state.db, account.id, "email", a).await?,
        None => {
            qb.push(" FROM profiles p WHERE p.account_id = ").push_bind(account.id);
            campaigns::push_eligible(&mut qb, "email");
        }
    }
    qb.push(" LIMIT ").push_bind(RULE_SAMPLE);
    let mut tx = state.db.begin().await?;
    sqlx::query("SET LOCAL statement_timeout = '10s'").execute(&mut *tx).await?;
    let people: Vec<crate::models::Profile> = qb.build_query_as().fetch_all(&mut *tx).await?;
    tx.commit().await?;
    let (state, account, content, saved) = (state.clone(), account.clone(), content.clone(), saved.clone());
    let started = Instant::now();
    let counts = tokio::task::spawn_blocking(move || {
        let (mut tried, mut hidden, mut errors) = (0i64, 0i64, 0i64);
        for p in &people {
            if started.elapsed() > AUDIENCE_DEADLINE {
                break;
            }
            let ctx = crate::render::context(&account, Some(p), None, "#unsubscribe");
            let (shows, errs) = super::prepare::shows_for(&state, &account, &content, &ctx, &saved);
            tried += 1;
            hidden += i64::from(!shows);
            errors += i64::from(errs > 0);
        }
        (tried, hidden, errors)
    })
    .await
    .map_err(|e| AppError::Internal(e.into()))?;
    Ok(counts)
}

/// Run the checks. `people`: the caller may read profiles; without it, nothing that counts
/// recipients or reads their data runs (those findings are left out, with a note saying so).
pub async fn run(state: &AppState, account_id: Uuid, input: Input, may_read_people: bool) -> AppResult<Value> {
    // One check at a time per account, before any of its work (audience counts included).
    let _slot = take_slot(account_id).await?;
    let account = campaigns::load_account(&state.db, account_id).await?;
    let ses = state.email.name() == "ses";
    let mut findings: Vec<Finding> = Vec::new();

    // What is being checked, and who it goes to.
    let campaign = match input.campaign_id {
        Some(id) => Some(campaigns::load(&state.db, account_id, id).await?),
        None => None,
    };
    let flow_content = match input.flow_id {
        Some(fid) => {
            // Ownership is checked even when unsaved content comes with it.
            let f = crate::api::messaging::load_flow(state, account_id, fid).await?;
            match (&input.content, input.node_id.as_deref()) {
                (Some(_), _) => None,
                (None, Some(nid)) => {
                    let def = crate::flows::parse_definition(&f.definition)?;
                    let node = def.nodes.iter().find(|n| n.id == nid && n.kind == "email").ok_or_else(|| AppError::not_found("email step"))?;
                    Some(node.data.get("content").cloned().unwrap_or(json!({})))
                }
                (None, None) => return Err(AppError::bad("give node_id: the flow's email step to check")),
            }
        }
        None => None,
    };
    if let Some(c) = &campaign {
        if c.channel != "email" {
            return Err(AppError::bad("checks are for email campaigns"));
        }
    }
    let content = input
        .content
        .clone()
        .or_else(|| campaign.as_ref().map(|c| c.content.clone()))
        .or(flow_content)
        .ok_or_else(|| AppError::bad("give content, a campaign_id, or a flow_id and node_id"))?;
    if !content.is_object() {
        return Err(AppError::bad("content must be an email (subject, preheader, blocks)"));
    }
    let is_flow = input.flow_id.is_some();
    let audience: Option<Audience> = match (&input.audience, &campaign) {
        (Some(a), _) => Some(serde_json::from_value(a.clone()).map_err(|e| AppError::bad(format!("invalid audience: {e}")))?),
        (None, Some(c)) => Some(serde_json::from_value(c.audience.clone()).unwrap_or_default()),
        _ => None,
    };

    // Problems the product already refuses to send with.
    if let Some(c) = &campaign {
        let mut c2 = c.clone();
        c2.content = content.clone();
        if let Some(a) = &audience {
            c2.audience = serde_json::to_value(a)?;
        }
        for p in campaigns::readiness(&account, &c2, ses) {
            findings.push(blocking("not_ready", p, "Fix this before sending; the campaign can't go out until you do."));
        }
    } else {
        if s(&content, "subject").trim().is_empty() {
            findings.push(blocking("subject_missing", "Add a subject line", "Emails without a subject can't be sent."));
        }
        let from = content.get("from_email").and_then(|v| v.as_str()).filter(|s| !s.is_empty()).map(str::to_string).or(account.from_email.clone());
        match &from {
            None => findings.push(blocking("no_sender", "Set a sender email in Settings → Sending", "Emails need a from address.")),
            Some(f) if ses => {
                if let Err(e) = crate::sending::check_sender_domain_name(&account, f) {
                    findings.push(blocking("domain_unverified", e, "Email from an unverified domain can't be sent."));
                }
            }
            _ => {}
        }
        if account.address.as_deref().unwrap_or("").trim().is_empty() {
            findings.push(blocking("no_address", "Add your mailing address in Settings (required by CAN-SPAM)", "Every marketing email must show a postal address."));
        }
    }

    // Render it the way a recipient would get it (tracked links included, for the size).
    let event = is_flow.then(|| json!({"properties": {}}));
    let mut ctx = sample_ctx(&account, event.as_ref());
    if let Err(e) = super::prepare::check_rules(&content) {
        findings.push(blocking("render_error", "This email can't be rendered", e));
    }
    let prepared = super::prepare::prepare_all(state, &account, &content, &ctx).await;
    let mut size = None;
    let mut prepared_content = content.clone();
    match prepared {
        Err(e) => findings.push(blocking("render_error", "This email can't be rendered", e.public_message())),
        Ok(p) => {
            prepared_content = p.clone();
            let personalized = crate::features::catalog::personalize(&state.db, &account, None, event.as_ref(), &p, &mut ctx, true).await.map(|c| c.into_owned());
            let rendered = personalized.and_then(|c| {
                let ec: crate::render::EmailContent = serde_json::from_value(c)?;
                let link_base = crate::domains::link_base(state, &account);
                let t = crate::render::Tracking { public_url: &link_base, secret: &state.cfg.secret_key, message_id: Uuid::nil(), unsubscribe_url: crate::sending::unsubscribe_url(state, &account, Uuid::nil()) };
                crate::render::render_email(&account, &ec, &ctx, Some(&t))
            });
            match rendered {
                Ok(r) => size = Some(r.html.len()),
                Err(e) => findings.push(blocking("render_error", "This email can't be rendered", e.public_message())),
            }
        }
    }

    // Subject and preview text.
    let subject = s(&content, "subject").trim();
    let preheader = s(&content, "preheader").trim();
    let letters: Vec<char> = subject.chars().filter(|c| c.is_alphabetic()).collect();
    if subject.chars().count() > 60 {
        findings.push(finding("subject_long", "warning", "Shorten the subject line", format!("It's {} characters; phones show about 40 and desktops about 60. Put the important words first.", subject.chars().count())));
    }
    if letters.len() >= 8 && letters.iter().filter(|c| c.is_uppercase()).count() * 10 >= letters.len() * 7 {
        findings.push(finding("subject_caps", "warning", "Don't write the subject in capitals", "All-caps subjects look like shouting and spam filters count them against you."));
    }
    if subject.contains("!!") || subject.contains("$$") {
        findings.push(finding("subject_punctuation", "warning", "Drop the repeated punctuation in the subject", "\"!!\" and \"$$\" are classic spam signals."));
    }
    if preheader.is_empty() {
        findings.push(finding("preheader_missing", "warning", "Add preview text", "Without it, inboxes show the first words of the email, which are often \"View in browser\" or an image's alt text."));
    } else if preheader.chars().count() > 120 {
        findings.push(finding("preheader_long", "info", "Preview text is long", "Inboxes show 40 to 120 characters of it; the rest is cut off."));
    } else if preheader.eq_ignore_ascii_case(subject) {
        findings.push(finding("preheader_repeats", "info", "Preview text repeats the subject", "Use it to add something the subject doesn't say."));
    }

    // Content.
    let sc = scan(&prepared_content);
    let no_alt: Vec<&(String, String, bool)> = sc.images.iter().filter(|(_, alt, _)| alt.trim().is_empty()).collect();
    if !no_alt.is_empty() {
        let linked = no_alt.iter().filter(|(_, _, l)| *l).count();
        findings.push(finding(
            "alt_missing",
            "warning",
            format!("Add alt text to {}", plural(no_alt.len(), "image", "images")),
            format!("Alt text is read aloud by screen readers and shown when images are blocked{}.", if linked > 0 { "; on a linked image it's the only thing that says where the link goes" } else { "" }),
        ));
    }
    let placeholders = sc.images.iter().filter(|(src, _, _)| src.contains("/e/assets/placeholder-")).count();
    if placeholders > 0 {
        findings.push(finding("placeholder_images", "warning", format!("Replace {}", plural(placeholders, "sample image", "sample images")), "The starter template's gray sample images are still in this email."));
    }
    if !sc.images.is_empty() && sc.text_chars < 60 {
        findings.push(finding("image_only", "warning", "Add some text", "An email that's mostly images is a spam signal, and says nothing when images are blocked. Put the key message in text too."));
    }
    if sc.empty_links > 0 {
        findings.push(finding("empty_links", "warning", format!("{} no destination", plural(sc.empty_links, "link or button has", "links or buttons have")), "Add the address each one should open."));
    }
    if !sc.bad_links.is_empty() {
        findings.push(finding("unsafe_links", "warning", format!("Fix {}", plural(sc.bad_links.len(), "link that email apps block", "links that email apps block")), "Links must start with https://, http://, mailto: or tel:."));
    }
    let shorteners: Vec<String> = sc.links.iter().filter_map(|(u, _)| host_of(u)).filter(|h| SHORTENERS.contains(&h.as_str())).collect::<HashSet<_>>().into_iter().collect();
    if !shorteners.is_empty() {
        findings.push(finding("shortener", "warning", "Use full links instead of link shorteners", format!("Spam filters distrust shortened links ({}). DumbMail tracks clicks on full links anyway.", shorteners.join(", "))));
    }
    let mismatch: Vec<&(String, String)> = sc.links.iter().filter(|(u, t)| mismatched(t, u)).collect();
    if let Some((u, t)) = mismatch.first() {
        findings.push(finding("link_text_mismatch", "warning", "Link text shows a different address than the link opens", format!("\"{}\" opens {}. Spam filters and readers both distrust this; make the text match or use words.", t, host_of(u).unwrap_or_default())));
    }
    if let Some(n) = size {
        if n > GMAIL_CLIP_BYTES {
            findings.push(finding("gmail_clip", "warning", "Gmail will cut this email off", format!("It's {} KB; Gmail hides everything after 102 KB behind \"View entire message\", including the unsubscribe link. Remove blocks or custom HTML.", n / 1024)));
        } else if n > GMAIL_CLIP_BYTES * 9 / 10 {
            findings.push(finding("gmail_clip_near", "info", "Close to Gmail's size limit", format!("It's {} KB; Gmail cuts emails off at 102 KB.", n / 1024)));
        }
    }

    // Who gets it, and personalization that would come out empty for some of them.
    let vars = bare_vars(&sc.strings);
    let saved = super::prepare::load_saved_for(state, &account, &content).await?;
    let rules = super::prepare::rules_text(&content, &saved);
    // Rules about the event can't be tried without one (flows): only rules about the person are.
    let try_rules = !rules.is_empty() && !(is_flow && rules.contains("event"));
    let mut rule_result = None;
    let (people, never, missing) = match &audience {
        _ if !may_read_people => {
            findings.push(finding("people_not_checked", "info", "Recipient counts weren't checked", "Counting who gets this email, and trying its display rules and personalization on them, needs permission to read profiles."));
            (None, 0, Vec::new())
        }
        Some(a) if a.include.is_empty() => (None, 0, Vec::new()),
        a => {
            let work = async {
                let _db = tokio::time::timeout(Duration::from_secs(5), DB_CHECKS.clone().acquire_owned())
                    .await
                    .map_err(|_| AppError::Busy("the server is busy checking other emails; try again in a minute".into()))?
                    .map_err(|e| AppError::Internal(e.into()))?;
                let c = counts(state, account_id, a.as_ref(), &vars).await?;
                let r = if try_rules { Some(rule_counts(state, &account, a.as_ref(), &content, &saved).await?) } else { None };
                Ok::<_, AppError>((c, r))
            };
            match tokio::time::timeout(AUDIENCE_DEADLINE + Duration::from_secs(5), work).await {
                Ok(Ok(((t, n, m), r))) => {
                    rule_result = r;
                    (Some(t), n, m)
                }
                Ok(Err(e)) => {
                    findings.push(finding("count_failed", "info", "Couldn't count recipients", e.public_message()));
                    (None, 0, Vec::new())
                }
                Err(_) => {
                    findings.push(finding("count_failed", "info", "Couldn't count recipients", "Counting took too long; try again later."));
                    (None, 0, Vec::new())
                }
            }
        }
    };
    if let Some((tried, hidden, errors)) = rule_result {
        let of = |n: i64| if tried >= RULE_SAMPLE { format!("{} of the first {tried} people checked", n) } else { plural(n as usize, "person", "people") };
        if hidden > 0 {
            findings.push(finding(
                "rules_hide_everything",
                "warning",
                format!("Display rules hide every block for {}", of(hidden)),
                "They'll be skipped (\"nothing to show for this person\") instead of getting an empty email. Add a block everyone sees, or loosen a rule, if they should get it.",
            ));
        }
        if errors > 0 {
            findings.push(finding(
                "rule_errors",
                "warning",
                format!("An advanced display rule can't be worked out for {}", of(errors)),
                "Its block is hidden for them. Guard values that may be missing with `is defined`, or use a simple rule.",
            ));
        }
    } else if !rules.is_empty() && is_flow && may_read_people {
        findings.push(finding("rules_event", "info", "Display rules depend on the event", "Check the email with \"Preview as\" and a sample event: rules about the event can't be tried on your audience ahead of time."));
    }
    if let (Some(0), Some(_)) = (people, &audience) {
        findings.push(blocking("audience_empty", "Nobody in this audience can get this email", "Everyone in it has unsubscribed, bounced, has no email address or hasn't confirmed. Pick another list or segment."));
    }
    if let (Some(t), Some(_)) = (people, &audience) {
        if never > 0 && t > 0 {
            findings.push(finding("never_subscribed", "info", format!("{} never subscribed", plural(never as usize, "recipient", "recipients")), "They're on your lists but never opted in to marketing (for example, imported customers). Make sure you have permission to email them."));
        }
    }
    for (i, v) in vars.iter().enumerate() {
        match v {
            Var::Event(path) if !is_flow => findings.push(finding("event_in_campaign", "warning", format!("{{{{ {path} }}}} is always empty in a campaign"), "Event data only exists in flows started by an event. Remove it, or add a fallback like |default('...')")),
            Var::Event(path) => findings.push(finding("event_no_default", "info", format!("Add a fallback to {{{{ {path} }}}}"), "It comes from the event that started the flow; if an event doesn't carry it, it comes out empty. Add |default('...').")),
            _ => {
                let n = missing.get(i).copied().unwrap_or(0);
                if n > 0 {
                    let whose = if audience.is_some() { "in this audience" } else { "who can get email" };
                    findings.push(finding("personalization_empty", "warning", format!("{{{{ {} }}}} is empty for {} {whose}", v.tag(), plural(n as usize, "person", "people")), format!("Add a fallback, like {{{{ {}|default('there') }}}}.", v.tag())));
                }
            }
        }
    }

    // Coupons: the check a campaign's launch runs (features::coupons::checks), on the email as it will
    // be sent (saved blocks as they are now). Whether there are enough codes needs the audience, so
    // that part runs only with permission to read people, behind the same guard and time limit.
    let now = chrono::Utc::now();
    let mut row = match &campaign {
        Some(c) => crate::models::CampaignRow { content: content.clone(), audience: audience.as_ref().map(serde_json::to_value).transpose()?.unwrap_or(Value::Null), ..c.clone() },
        None => crate::models::CampaignRow {
            id: Uuid::nil(),
            account_id,
            name: String::new(),
            channel: "email".into(),
            status: "draft".into(),
            audience: Value::Null,
            content: content.clone(),
            send_at: None,
            sent_at: None,
            error: None,
            created_at: now,
            updated_at: now,
            ab_test: None,
            ab_test_ends_at: None,
            ab_winner: None,
            send_options: None,
        },
    };
    if !may_read_people {
        row.audience = Value::Null;
    }
    let row = super::prepare::with_saved_filled(state, &row).await?;
    let coupons = crate::features::coupons::template::may_use_coupons(&row.content) || row.ab_test.as_ref().is_some_and(crate::features::coupons::template::may_use_coupons);
    if coupons {
        let counts_people = !row.audience.is_null();
        let work = async {
            let _db = if counts_people {
                Some(
                    tokio::time::timeout(Duration::from_secs(5), DB_CHECKS.clone().acquire_owned())
                        .await
                        .map_err(|_| AppError::Busy("the server is busy checking other emails; try again in a minute".into()))?
                        .map_err(|e| AppError::Internal(e.into()))?,
                )
            } else {
                None
            };
            crate::features::coupons::checks::campaign_check(state, account_id, &row).await
        };
        match tokio::time::timeout(AUDIENCE_DEADLINE + Duration::from_secs(5), work).await {
            Ok(Ok(c)) => {
                for p in c.problems {
                    findings.push(blocking("coupon_problem", p, "Emails that use this coupon can't be sent until you fix it."));
                }
                for w in c.warnings {
                    findings.push(finding("coupon_short", "warning", w, "When a coupon runs out, its emails wait and go out once you add codes."));
                }
            }
            Ok(Err(e)) => findings.push(finding("coupon_unchecked", "info", "Couldn't check the coupons", e.public_message())),
            Err(_) => findings.push(finding("coupon_unchecked", "info", "Couldn't check the coupons", "Checking took too long; try again later.")),
        }
    }

    // Sending domain (when the product doesn't already require it).
    if !ses {
        let from = content.get("from_email").and_then(|v| v.as_str()).filter(|s| !s.is_empty()).map(str::to_string).or(account.from_email.clone());
        if let Some(f) = from {
            if crate::sending::check_sender_domain_name(&account, &f).is_err() {
                findings.push(finding("domain_unauthenticated", "warning", "Authenticate your sending domain", "Add the DKIM and SPF records in Settings → Domains so inboxes trust mail from it."));
            }
        }
    }

    // Links and images, over the network.
    let mut links_out = Vec::new();
    if !input.skip_links {
        let web = |u: &str| (u.starts_with("https://") || u.starts_with("http://")) && !u.contains("{{") && !u.contains("{%");
        let found: Vec<(String, bool)> = sc.links.iter().map(|(u, _)| (u.clone(), false)).chain(sc.images.iter().map(|(u, _, _)| (u.clone(), true))).filter(|(u, _)| web(u)).collect();
        // Hosts of the addresses and of where tracked links lead, so a tracked link to another
        // account's link domain is recognized as DumbMail's too.
        let hosts: HashSet<String> = found.iter().flat_map(|(u, _)| [exact_host(u), tracked_destination(u).as_deref().and_then(exact_host)]).flatten().collect();
        let own = own_hosts(state, &hosts).await?;
        let mut urls: Vec<(String, bool)> = Vec::new();
        let mut own_images: Vec<Uuid> = Vec::new();
        let mut not_checked: Vec<(String, bool)> = Vec::new();
        for (u, img) in found {
            match classify(&u, &own) {
                Target::External(dest) => {
                    if !urls.iter().any(|(x, i)| *x == dest && *i == img) {
                        urls.push((dest, img));
                    }
                }
                Target::OwnImage(id) if img => own_images.push(id),
                Target::OwnImage(_) | Target::Own => {
                    if !not_checked.iter().any(|(x, _)| *x == u) {
                        not_checked.push((u, img));
                    }
                }
            }
        }
        let skipped = urls.len().saturating_sub(MAX_URLS);
        urls.truncate(MAX_URLS);
        let heavy_own: Vec<(Uuid, i32)> = if own_images.is_empty() {
            Vec::new()
        } else {
            sqlx::query_as("SELECT id, size FROM images WHERE id = ANY($1) AND account_id = $2 AND size > $3").bind(&own_images).bind(account_id).bind(HEAVY_IMAGE_BYTES as i32).fetch_all(&state.db).await?
        };
        let results = probe_all(account_id, urls).await;
        let broken: Vec<&(String, bool, Probe)> = results.iter().filter(|(_, _, p)| p.ok == Some(false)).collect();
        let unsure = results.iter().filter(|(_, _, p)| p.ok.is_none()).count();
        let heavy = results.iter().filter(|(_, img, p)| *img && p.bytes.is_some_and(|b| b > HEAVY_IMAGE_BYTES)).count() + heavy_own.len();
        let (bad_links, bad_images): (Vec<_>, Vec<_>) = broken.iter().copied().partition(|(_, img, _)| !*img);
        if !bad_links.is_empty() {
            findings.push(finding("broken_links", "warning", format!("{} broken", plural(bad_links.len(), "link looks", "links look")), bad_links.iter().map(|(u, _, p)| format!("{u}: {}", p.note)).collect::<Vec<_>>().join("\n")));
        }
        if !bad_images.is_empty() {
            findings.push(finding("broken_images", "warning", format!("{} can't be loaded", plural(bad_images.len(), "image", "images")), bad_images.iter().map(|(u, _, p)| format!("{u}: {}", p.note)).collect::<Vec<_>>().join("\n")));
        }
        if unsure > 0 {
            findings.push(finding("links_unchecked", "info", format!("Couldn't confirm {}", plural(unsure, "link", "links")), "Some sites refuse automated checks or answered too slowly. Open them yourself to be sure."));
        }
        if heavy > 0 {
            findings.push(finding("heavy_images", "warning", format!("{} over 1 MB", plural(heavy, "image is", "images are")), "Large images load slowly on phones. Aim for under 500 KB: resize to at most 1200 px wide and save as JPEG."));
        }
        if skipped > 0 {
            findings.push(finding("links_skipped", "info", format!("Checked the first {MAX_URLS} links and images"), format!("{skipped} more weren't checked.")));
        }
        links_out = results
            .iter()
            .map(|(u, img, p)| json!({"url": u, "kind": if *img { "image" } else { "link" }, "ok": p.ok, "status": p.status, "note": p.note, "bytes": p.bytes}))
            .chain(not_checked.iter().map(|(u, img)| json!({"url": u, "kind": if *img { "image" } else { "link" }, "ok": null, "status": null, "note": "DumbMail link, not checked", "bytes": null})))
            .collect();
    }

    let rank = |f: &Finding| match f.severity {
        "error" => 0,
        "warning" => 1,
        _ => 2,
    };
    findings.sort_by_key(rank);
    let errors = findings.iter().filter(|f| f.severity == "error").count();
    let warnings = findings.iter().filter(|f| f.severity == "warning").count();
    Ok(json!({
        "ok": errors == 0,
        "errors": errors,
        "warnings": warnings,
        "findings": findings.iter().map(|f| json!({"id": f.id, "severity": f.severity, "title": f.title, "detail": f.detail, "blocks_sending": f.blocks_sending})).collect::<Vec<_>>(),
        "size_bytes": size,
        "recipients": people,
        "links": links_out,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_finished_check_gives_its_slot_back_at_once() {
        let id = Uuid::now_v7();
        let slot = take_slot(id).await.unwrap();
        assert!(take_slot(id).await.is_err(), "one check at a time per account");
        drop(slot);
        // No waiting in between: the very next check must get the slot.
        assert!(take_slot(id).await.is_ok());
    }


    #[test]
    fn finds_bare_personalization() {
        let strings = vec![
            "Hi {{ first_name }} and {{ person.first_name|default('x') }}".to_string(),
            "{{ person.properties.city }} {{ favorite_color }} {{ organization.name }}".to_string(),
            "{% for line in event.properties.items %}{{ line.name }}{% endfor %} {{ event.properties.order_id }}".to_string(),
            "{{ item.name }} {{ unsubscribe_url }}".to_string(),
        ];
        let vars = bare_vars(&strings);
        assert_eq!(vars, vec![Var::Field("first_name"), Var::Property("city".into()), Var::Property("favorite_color".into()), Var::Event("event.properties.order_id".into())]);
    }

    #[test]
    fn spots_mismatched_link_text() {
        assert!(mismatched("www.mybank.com", "https://evil.test/login"));
        assert!(mismatched("https://shop.acme.com", "https://other.test"));
        assert!(!mismatched("shop.acme.com", "https://shop.acme.com/sale"));
        assert!(!mismatched("acme.com", "https://shop.acme.com"));
        assert!(!mismatched("Shop now", "https://evil.test"));
    }

    #[test]
    fn scans_blocks_for_links_images_and_text() {
        let c = json!({"subject": "s", "blocks": [
            {"type": "image", "src": "https://x.test/a.png", "alt": "", "href": "https://x.test"},
            {"type": "button", "text": "Go", "href": "https://"},
            {"type": "text", "html": "<p>Read <a href=\"javascript:alert(1)\">this</a> and <a href='https://bit.ly/x'>that</a></p>"},
            {"type": "columns", "columns": [[{"type": "image", "src": "https://x.test/b.png", "alt": "B"}]]}
        ]});
        let sc = scan(&c);
        assert_eq!(sc.images.len(), 2);
        assert_eq!(sc.empty_links, 1);
        assert_eq!(sc.bad_links, vec!["javascript:alert(1)".to_string()]);
        assert!(sc.links.iter().any(|(u, t)| u == "https://bit.ly/x" && t == "that"));
    }

    #[test]
    fn dumbmail_links_are_never_requested() {
        let own: HashSet<String> = ["pl.test", "links.acme.test"].into_iter().map(String::from).collect();
        assert_eq!(classify("https://links.acme.test/t/o/abc", &own), Target::Own, "a copied open pixel");
        assert_eq!(classify("https://LINKS.acme.test/u/abc", &own), Target::Own);
        assert_eq!(classify("https://links.acme.test/t/c/abc?u=https%3A%2F%2Fshop.test%2Fx&s=1", &own), Target::External("https://shop.test/x".into()), "a tracked link is checked as where it leads");
        assert_eq!(classify("https://links.acme.test/t/c/abc?u=https%3A%2F%2Fpl.test%2Fu%2Fx&s=1", &own), Target::Own);
        assert_eq!(classify("https://links.acme.test/t/c/abc?u=javascript%3Ax&s=1", &own), Target::Own);
        let id = Uuid::now_v7();
        assert_eq!(classify(&format!("https://pl.test/i/{id}"), &own), Target::OwnImage(id));
        assert_eq!(classify("https://shop.test/a", &own), Target::External("https://shop.test/a".into()));
        assert_eq!(classify("https://links.acme.test.evil.test/t/o/x", &own), Target::External("https://links.acme.test.evil.test/t/o/x".into()));
    }

    #[test]
    fn images_must_be_images_and_redirects_are_unverified() {
        let r = |status: u16, ct: &str, body: &[u8]| net::Response { status, content_type: ct.into(), content_length: None, location: None, body: body.to_vec() };
        assert_eq!(assess(&r(200, "text/html; charset=utf-8", b"<html>"), true).ok, Some(false));
        assert_eq!(assess(&r(200, "", b"<html>"), true).ok, Some(false));
        assert_eq!(assess(&r(200, "", b"\x89PNG\r\n\x1a\nrest"), true).ok, Some(true));
        assert_eq!(assess(&r(200, "image/jpeg", b""), true).ok, Some(true));
        assert_eq!(assess(&r(200, "text/html", b"<html>"), false).ok, Some(true), "a page is fine for a link");
        let moved = assess(&r(302, "", b""), false);
        assert_eq!(moved.ok, None);
        assert!(moved.note.contains("not followed"));
    }

    #[test]
    fn judges_answers() {
        assert_eq!(judge(200).0, Some(true));
        assert_eq!(judge(301).0, None, "a redirect isn't followed, so it isn't verified");
        assert_eq!(judge(404).0, Some(false));
        assert_eq!(judge(403).0, None);
        assert_eq!(judge(503).0, Some(false));
    }
}
