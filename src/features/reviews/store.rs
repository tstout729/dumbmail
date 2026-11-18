//! Review data: settings, the things reviewed (with their published summary), reviews and
//! moderation, flags and the honesty rules.
//!
//! The summary on `review_subjects` (count, rating sum, histogram) changes only in the transaction
//! that moves a review into or out of `published` ([`set_status`]), so widgets never count rows.
//! [`rebuild_summaries`] recomputes it from the reviews under the same locks.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use once_cell::sync::Lazy;
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sqlx::{PgConnection, PgPool};
use uuid::Uuid;

use crate::error::{AppError, AppResult};

pub const STATUSES: [&str; 4] = ["pending", "published", "rejected", "removed"];
/// Why a business may reject a review. Rejecting is never about the rating.
pub const REJECT_REASONS: [&str; 6] = ["spam", "offensive", "personal_info", "not_about_this", "fake", "other"];
pub const MAX_TITLE: usize = 150;
pub const MAX_BODY: usize = 5000;
pub const MAX_NAME: usize = 60;
pub const MAX_REPLY: usize = 2000;
pub const MAX_QUESTIONS: usize = 3;
pub const MAX_ANSWER: usize = 200;
pub const MAX_FLAG_WORDS: usize = 200;

// ---------- Settings ----------

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct Settings {
    pub auto_publish: bool,
    pub auto_publish_min_rating: i16,
    pub flag_words: Vec<String>,
    pub questions: Value,
    pub public_reviews: bool,
    pub request_days: i32,
    pub thank_you_coupon: Option<String>,
    pub subscribe_form_id: Option<Uuid>,
    pub widget: Value,
    /// More metrics that mean a real purchase or stay, for verified reviews.
    pub purchase_metrics: Vec<String>,
}

pub async fn settings(db: &PgPool, account_id: Uuid) -> AppResult<Settings> {
    sqlx::query("INSERT INTO review_settings (account_id) VALUES ($1) ON CONFLICT DO NOTHING").bind(account_id).execute(db).await?;
    Ok(sqlx::query_as(
        "SELECT auto_publish, auto_publish_min_rating, flag_words, questions, public_reviews, request_days, thank_you_coupon, subscribe_form_id, widget, purchase_metrics
         FROM review_settings WHERE account_id = $1",
    )
    .bind(account_id)
    .fetch_one(db)
    .await?)
}

fn nullable<'de, D: serde::Deserializer<'de>, T: Deserialize<'de>>(d: D) -> Result<Option<Option<T>>, D::Error> {
    Ok(Some(Option::<T>::deserialize(d)?))
}

/// A change to the settings. Fields left out stay; `null` clears the nullable ones.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SettingsPatch {
    pub auto_publish: Option<bool>,
    pub auto_publish_min_rating: Option<i16>,
    pub flag_words: Option<Vec<String>>,
    pub questions: Option<Value>,
    pub public_reviews: Option<bool>,
    pub request_days: Option<i32>,
    #[serde(default, deserialize_with = "nullable")]
    pub thank_you_coupon: Option<Option<String>>,
    #[serde(default, deserialize_with = "nullable")]
    pub subscribe_form_id: Option<Option<Uuid>>,
    pub widget: Option<Value>,
    pub purchase_metrics: Option<Vec<String>>,
}

/// Questions: at most 3, each `{id, label, kind: "choice"|"text", options}` (2 to 6 options for a choice).
pub fn check_questions(v: &Value) -> AppResult<Value> {
    let list = v.as_array().ok_or_else(|| AppError::bad("questions must be a list"))?;
    if list.len() > MAX_QUESTIONS {
        return Err(AppError::bad(format!("ask at most {MAX_QUESTIONS} questions")));
    }
    let mut out = Vec::new();
    for (i, q) in list.iter().enumerate() {
        let label = q["label"].as_str().map(str::trim).unwrap_or("");
        if label.is_empty() || label.chars().count() > 80 {
            return Err(AppError::bad(format!("question {}: the label must be 1 to 80 characters", i + 1)));
        }
        let kind = q["kind"].as_str().unwrap_or("text");
        let id = q["id"].as_str().map(str::trim).filter(|s| !s.is_empty() && s.len() <= 40 && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')).map(String::from).unwrap_or_else(|| format!("q{}", i + 1));
        match kind {
            "text" => out.push(json!({"id": id, "label": label, "kind": "text"})),
            "choice" => {
                let opts: Vec<String> = q["options"].as_array().into_iter().flatten().filter_map(|o| o.as_str()).map(|o| o.trim().to_string()).filter(|o| !o.is_empty()).collect();
                if !(2..=6).contains(&opts.len()) || opts.iter().any(|o| o.chars().count() > 40) {
                    return Err(AppError::bad(format!("question {}: give 2 to 6 choices of up to 40 characters", i + 1)));
                }
                out.push(json!({"id": id, "label": label, "kind": "choice", "options": opts}));
            }
            other => return Err(AppError::bad(format!("question {}: kind must be choice or text (got {other:?})", i + 1))),
        }
    }
    let mut ids: Vec<&str> = out.iter().filter_map(|q| q["id"].as_str()).collect();
    ids.sort_unstable();
    ids.dedup();
    if ids.len() != out.len() {
        return Err(AppError::bad("question ids must be different"));
    }
    Ok(Value::Array(out))
}

fn check_widget(w: &Value) -> AppResult<Value> {
    let o = w.as_object().ok_or_else(|| AppError::bad("widget must be an object"))?;
    let mut out = serde_json::Map::new();
    for (k, v) in o {
        match k.as_str() {
            "star_color" | "text_color" | "accent_color" => {
                let c = v.as_str().unwrap_or("");
                if !c.is_empty() && crate::render::safe_color(c, "") != c {
                    return Err(AppError::bad(format!("widget {k} must be a hex color like #111827")));
                }
                out.insert(k.clone(), json!(c));
            }
            "heading" => {
                let s = v.as_str().unwrap_or("").trim();
                if s.chars().count() > 80 {
                    return Err(AppError::bad("widget heading must be at most 80 characters"));
                }
                out.insert(k.clone(), json!(s));
            }
            "per_page" => {
                let n = v.as_i64().unwrap_or(5);
                if !(3..=20).contains(&n) {
                    return Err(AppError::bad("widget per_page must be 3 to 20"));
                }
                out.insert(k.clone(), json!(n));
            }
            other => return Err(AppError::bad(format!("unknown widget setting {other:?} (use heading, star_color, text_color, accent_color, per_page)"))),
        }
    }
    Ok(Value::Object(out))
}

pub async fn update_settings(db: &PgPool, account_id: Uuid, p: SettingsPatch) -> AppResult<Settings> {
    let cur = settings(db, account_id).await?;
    let min = p.auto_publish_min_rating.unwrap_or(cur.auto_publish_min_rating);
    if !(1..=5).contains(&min) {
        return Err(AppError::bad("auto_publish_min_rating must be 1 to 5 stars"));
    }
    let days = p.request_days.unwrap_or(cur.request_days);
    if !(7..=365).contains(&days) {
        return Err(AppError::bad("review links must last 7 to 365 days"));
    }
    let words = match p.flag_words {
        Some(w) => {
            let mut out: Vec<String> = w.iter().map(|x| x.trim().to_lowercase()).filter(|x| !x.is_empty()).collect();
            out.sort();
            out.dedup();
            if out.len() > MAX_FLAG_WORDS || out.iter().any(|x| x.chars().count() > 40) {
                return Err(AppError::bad(format!("list at most {MAX_FLAG_WORDS} flag words of up to 40 characters")));
            }
            out
        }
        None => cur.flag_words.clone(),
    };
    let questions = match &p.questions {
        Some(q) => check_questions(q)?,
        None => cur.questions.clone(),
    };
    let coupon = match p.thank_you_coupon {
        Some(Some(name)) if !name.trim().is_empty() => {
            let c = crate::features::coupons::store::by_name(db, account_id, name.trim()).await?.ok_or_else(|| AppError::bad(format!("no coupon is called {name:?}")))?;
            Some(c.name)
        }
        Some(_) => None,
        None => cur.thank_you_coupon.clone(),
    };
    let form = match p.subscribe_form_id {
        Some(Some(id)) => {
            let ok: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM forms WHERE id = $1 AND account_id = $2 AND list_id IS NOT NULL)").bind(id).bind(account_id).fetch_one(db).await?;
            if !ok {
                return Err(AppError::bad("subscribe_form_id must be one of your sign-up forms that adds people to a list"));
            }
            Some(id)
        }
        Some(None) => None,
        None => cur.subscribe_form_id,
    };
    let widget = match &p.widget {
        Some(w) => check_widget(w)?,
        None => cur.widget.clone(),
    };
    let metrics = match p.purchase_metrics {
        Some(m) => {
            let mut out: Vec<String> = m.iter().map(|x| x.trim().to_string()).filter(|x| !x.is_empty()).collect();
            out.sort();
            out.dedup();
            if out.len() > 20 || out.iter().any(|x| x.chars().count() > 100) {
                return Err(AppError::bad("list at most 20 purchase metrics of up to 100 characters"));
            }
            out
        }
        None => cur.purchase_metrics.clone(),
    };
    sqlx::query(
        "UPDATE review_settings SET auto_publish = $2, auto_publish_min_rating = $3, flag_words = $4, questions = $5, public_reviews = $6,
            request_days = $7, thank_you_coupon = $8, subscribe_form_id = $9, widget = $10, purchase_metrics = $11, updated_at = now() WHERE account_id = $1",
    )
    .bind(account_id)
    .bind(p.auto_publish.unwrap_or(cur.auto_publish))
    .bind(min)
    .bind(&words)
    .bind(&questions)
    .bind(p.public_reviews.unwrap_or(cur.public_reviews))
    .bind(days)
    .bind(&coupon)
    .bind(form)
    .bind(&widget)
    .bind(&metrics)
    .execute(db)
    .await?;
    settings(db, account_id).await
}

// ---------- Subjects ----------

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct Subject {
    pub id: Uuid,
    pub external_id: String,
    pub kind: String,
    pub name: String,
    pub image_url: Option<String>,
    pub url: Option<String>,
    /// Each field is trusted only when the catalog, the API, an integration or a server-recorded
    /// event supplied it (never a browser).
    pub name_trusted: bool,
    pub image_trusted: bool,
    pub url_trusted: bool,
    pub review_count: i32,
    pub rating_sum: i64,
    pub rating_counts: Vec<i32>,
    /// Published reviews collected here (not imported).
    pub native_count: i32,
    pub native_sum: i64,
    pub updated_at: DateTime<Utc>,
}

pub const SUBJECT_COLS: &str = "s.id, s.external_id, s.kind, s.name, s.image_url, s.url, s.name_trusted, s.image_trusted, s.url_trusted, s.review_count, s.rating_sum, s.rating_counts, s.native_count, s.native_sum, s.updated_at";

impl Subject {
    pub fn average(&self) -> Option<f64> {
        (self.review_count > 0).then(|| (self.rating_sum as f64 / f64::from(self.review_count) * 10.0).round() / 10.0)
    }

    /// The average of the reviews collected here (what structured data may mark up).
    pub fn native_average(&self) -> Option<f64> {
        (self.native_count > 0).then(|| (self.native_sum as f64 / f64::from(self.native_count) * 10.0).round() / 10.0)
    }

    /// The published summary, as widgets and emails show it.
    pub fn summary(&self) -> Value {
        json!({"average": self.average(), "count": self.review_count, "histogram": {"5": self.rating_counts.get(4), "4": self.rating_counts.get(3), "3": self.rating_counts.get(2), "2": self.rating_counts.get(1), "1": self.rating_counts.first()}})
    }

    /// The name, for others to see: only when a trusted source supplied it.
    pub fn trusted_name(&self) -> Option<&str> {
        self.name_trusted.then_some(self.name.as_str())
    }

    /// The image, to load anywhere: only when a trusted source supplied it.
    pub fn trusted_image(&self) -> Option<&str> {
        self.image_url.as_deref().filter(|_| self.image_trusted)
    }

    /// The page address, to link anywhere: only when a trusted source supplied it.
    pub fn trusted_url(&self) -> Option<&str> {
        self.url.as_deref().filter(|_| self.url_trusted)
    }

    /// What the public sees about it: each field only when a trusted source supplied it.
    pub fn public_card(&self) -> Value {
        json!({"product_id": self.external_id, "kind": self.kind, "name": self.trusted_name(), "image_url": self.trusted_image(), "url": self.trusted_url()})
    }
}

/// What a request or the API says about the thing reviewed.
#[derive(Debug, Clone, Default)]
pub struct Hint {
    pub name: Option<String>,
    pub image_url: Option<String>,
    pub url: Option<String>,
    pub kind: Option<String>,
    /// Came from the account (catalog, API, integration, server event): the fields it supplies are
    /// trusted. Browser input never overwrites a trusted field, and a trusted hint only vouches
    /// for the fields it actually supplies.
    pub trusted: bool,
}

fn clean(s: Option<&str>, max: usize) -> Option<String> {
    s.map(str::trim).filter(|s| !s.is_empty()).map(|s| s.chars().take(max).collect())
}

fn http_url(s: Option<&str>) -> Option<String> {
    clean(s, 1000).filter(|u| url::Url::parse(u).is_ok_and(|p| matches!(p.scheme(), "http" | "https")))
}

pub fn valid_key(key: &str) -> Option<String> {
    let k = key.trim();
    (!k.is_empty() && k.chars().count() <= 255 && !k.chars().any(char::is_control)).then(|| k.to_string())
}

/// The subject for `key` (the store's product id or the account's own key), created or refreshed,
/// field by field. A published catalog product with that id supplies the name, image and address:
/// its name is trusted once the catalog has verified it, its image and address only when they came
/// from a trusted source (a feed, the API, a CSV or an integration), never from a browser: the
/// catalog marks each field a browser supplied (`url_from_browser`, `image_from_browser`), and the
/// mark stays until a trusted source sends that field itself.
/// Otherwise `hint` does. A field is replaced by an
/// untrusted value only while it isn't trusted, and becomes trusted only when a trusted source
/// supplies that field itself.
pub async fn subject_for(db: &PgPool, account_id: Uuid, key: &str, hint: &Hint) -> AppResult<Subject> {
    let key = valid_key(key).ok_or_else(|| AppError::bad("product_id must be 1 to 255 characters"))?;
    let product: Option<(String, Option<String>, Option<String>, bool, bool, bool)> = sqlx::query_as(
        "SELECT title, image_url, url, verified, NOT image_from_browser, NOT url_from_browser FROM catalog_products WHERE account_id = $1 AND external_id = $2 AND published",
    )
            .bind(account_id)
            .bind(&key)
            .fetch_optional(db)
            .await?;
    let kind = hint.kind.as_deref().filter(|k| ["product", "listing", "service"].contains(k)).unwrap_or("product");
    let (name, image, url, trusted, image_trusted, url_trusted) = match &product {
        Some((t, i, u, v, image_ok, url_ok)) => (clean(Some(t), 200), http_url(i.as_deref()), http_url(u.as_deref()), *v, *v && *image_ok, *v && *url_ok),
        None => (clean(hint.name.as_deref(), 200), http_url(hint.image_url.as_deref()), http_url(hint.url.as_deref()), hint.trusted, hint.trusted, hint.trusted),
    };
    let id = Uuid::now_v7();
    // $5..$7: the values (NULL: not supplied). $8: whether this source's name is trusted, $10 its
    // image, $11 its address.
    let q = format!(
        "INSERT INTO review_subjects AS s (id, account_id, external_id, kind, name, image_url, url, name_trusted, image_trusted, url_trusted)
         VALUES ($1, $2, $3, $4, COALESCE($5, $3), $6, $7, $8 AND $5::text IS NOT NULL, $10 AND $6::text IS NOT NULL, $11 AND $7::text IS NOT NULL)
         ON CONFLICT (account_id, external_id) DO UPDATE SET
            name = CASE WHEN $5::text IS NOT NULL AND ($8 OR NOT s.name_trusted) THEN $5 ELSE s.name END,
            image_url = CASE WHEN $6::text IS NOT NULL AND ($10 OR NOT s.image_trusted) THEN $6 ELSE s.image_url END,
            url = CASE WHEN $7::text IS NOT NULL AND ($11 OR NOT s.url_trusted) THEN $7 ELSE s.url END,
            name_trusted = s.name_trusted OR ($8 AND $5::text IS NOT NULL),
            image_trusted = s.image_trusted OR ($10 AND $6::text IS NOT NULL),
            url_trusted = s.url_trusted OR ($11 AND $7::text IS NOT NULL),
            kind = CASE WHEN $9 THEN EXCLUDED.kind ELSE s.kind END,
            updated_at = now()
         RETURNING {SUBJECT_COLS}"
    );
    Ok(sqlx::query_as(&q)
        .bind(id)
        .bind(account_id)
        .bind(&key)
        .bind(kind)
        .bind(&name)
        .bind(&image)
        .bind(&url)
        .bind(trusted)
        .bind(hint.kind.is_some() && hint.trusted)
        .bind(image_trusted)
        .bind(url_trusted)
        .fetch_one(db)
        .await?)
}

pub async fn find_subject(db: &PgPool, account_id: Uuid, key: &str) -> AppResult<Option<Subject>> {
    let q = format!("SELECT {SUBJECT_COLS} FROM review_subjects s WHERE s.account_id = $1 AND (s.external_id = $2 OR s.id::text = $2) LIMIT 1");
    Ok(sqlx::query_as(&q).bind(account_id).bind(key.trim()).fetch_optional(db).await?)
}

pub async fn subject_by_id(db: &PgPool, id: Uuid) -> AppResult<Subject> {
    let q = format!("SELECT {SUBJECT_COLS} FROM review_subjects s WHERE s.id = $1");
    sqlx::query_as(&q).bind(id).fetch_optional(db).await?.ok_or_else(|| AppError::not_found("product"))
}

// ---------- Flags ----------

static EMAIL: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)[a-z0-9._%+-]+@[a-z0-9.-]+\.[a-z]{2,}").unwrap());
static PHONE: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?:\+?\d[\s().-]?){9,}\d").unwrap());
static URL: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)\b(?:https?://|www\.)\S+|\b[a-z0-9-]+\.(?:com|net|org|io|co|shop|store|xyz|info|biz|ly|me)\b").unwrap());
/// A short built-in list of words that always hold a review for a person to look at.
const PROFANITY: &[&str] = &["fuck", "fucking", "shit", "bitch", "cunt", "asshole", "bastard", "dick", "pussy", "motherfucker", "slut", "whore", "nigger", "faggot", "retard"];

fn words(s: &str) -> Vec<String> {
    s.to_lowercase().split(|c: char| !c.is_alphanumeric() && c != '\'').filter(|w| !w.is_empty()).map(String::from).collect()
}

/// Why a review's text needs a person to look at it: `profanity`, `email`, `phone`, `link` and
/// `word:<the account's word>`.
pub fn flags(text: &str, account_words: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    let ws = words(text);
    if ws.iter().any(|w| PROFANITY.contains(&w.as_str())) {
        out.push("profanity".to_string());
    }
    if EMAIL.is_match(text) {
        out.push("email".into());
    }
    if PHONE.is_match(text) {
        out.push("phone".into());
    }
    if URL.is_match(&EMAIL.replace_all(text, " ")) {
        out.push("link".into());
    }
    let lower = text.to_lowercase();
    for w in account_words {
        let hit = if w.contains(' ') { lower.contains(w.as_str()) } else { ws.iter().any(|x| x == w) };
        if hit {
            out.push(format!("word:{w}"));
        }
    }
    out
}

/// Whether a review goes live without a person: verified, at or above the account's stars, nothing flagged.
pub fn auto_publishes(s: &Settings, verified: bool, rating: i16, flags: &[String]) -> bool {
    s.auto_publish && verified && rating >= s.auto_publish_min_rating && flags.is_empty()
}

/// The name shown when the reviewer gave none: never anything from their profile. "A guest" for a
/// stay, "A verified buyer" for a verified purchase, otherwise "A customer".
pub fn default_name(verified: bool, stay: bool) -> &'static str {
    match (stay, verified) {
        (true, _) => "A guest",
        (false, true) => "A verified buyer",
        (false, false) => "A customer",
    }
}

/// Plain text: trimmed, control characters (other than new lines) removed, at most `max` characters.
pub fn plain(s: &str, max: usize) -> String {
    let t: String = s.replace("\r\n", "\n").chars().filter(|c| !c.is_control() || *c == '\n').collect();
    let t = t.trim();
    let mut out: String = t.chars().take(max).collect();
    while out.contains("\n\n\n") {
        out = out.replace("\n\n\n", "\n\n");
    }
    out
}

/// Answers to the account's questions: only known ids, a listed choice or bounded text.
pub fn check_answers(questions: &Value, given: &Value) -> AppResult<Value> {
    let mut out = Vec::new();
    let Some(given) = given.as_object() else { return Ok(json!([])) };
    for q in questions.as_array().into_iter().flatten() {
        let id = q["id"].as_str().unwrap_or("");
        let Some(a) = given.get(id).and_then(|v| v.as_str()).map(str::trim).filter(|s| !s.is_empty()) else { continue };
        match q["kind"].as_str() {
            Some("choice") => {
                if !q["options"].as_array().is_some_and(|o| o.iter().any(|x| x.as_str() == Some(a))) {
                    return Err(AppError::bad(format!("pick one of the listed answers for {:?}", q["label"].as_str().unwrap_or(""))));
                }
                out.push(json!({"id": id, "label": q["label"], "answer": a}));
            }
            _ => out.push(json!({"id": id, "label": q["label"], "answer": plain(a, MAX_ANSWER)})),
        }
    }
    Ok(Value::Array(out))
}

// ---------- Reviews ----------

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct Review {
    pub id: Uuid,
    pub account_id: Uuid,
    pub subject_id: Uuid,
    pub profile_id: Option<Uuid>,
    pub request_id: Option<Uuid>,
    pub status: String,
    pub reject_reason: Option<String>,
    pub rating: i16,
    pub title: String,
    pub body: String,
    pub display_name: String,
    pub recommend: Option<bool>,
    pub answers: Value,
    pub verified: bool,
    /// Imports: the source said it was verified (shown in the dashboard only, never as a badge).
    pub source_verified: bool,
    pub source: String,
    pub incentivized: bool,
    pub flags: Vec<String>,
    pub featured: bool,
    pub reply: Option<String>,
    pub replied_at: Option<DateTime<Utc>>,
    pub confirmed_at: Option<DateTime<Utc>>,
    pub reviewed_at: DateTime<Utc>,
    pub created_at: DateTime<Utc>,
    pub edited_at: Option<DateTime<Utc>>,
    pub published_at: Option<DateTime<Utc>>,
    pub moderated_at: Option<DateTime<Utc>>,
    pub import_id: Option<Uuid>,
}

pub const REVIEW_COLS: &str = "r.id, r.account_id, r.subject_id, r.profile_id, r.request_id, r.status, r.reject_reason, r.rating, r.title, r.body, r.display_name,
    r.recommend, r.answers, r.verified, r.source_verified, r.source, r.incentivized, r.flags, r.featured, r.reply, r.replied_at, r.confirmed_at, r.reviewed_at, r.created_at,
    r.edited_at, r.published_at, r.moderated_at, r.import_id";

pub async fn get(db: &PgPool, account_id: Uuid, id: Uuid) -> AppResult<Review> {
    let q = format!("SELECT {REVIEW_COLS} FROM reviews r WHERE r.id = $1 AND r.account_id = $2");
    sqlx::query_as(&q).bind(id).bind(account_id).fetch_optional(db).await?.ok_or_else(|| AppError::not_found("review"))
}

/// Add `sign` (+1 or -1) reviews of `rating` stars to a subject's published summary (and to the
/// summary of reviews collected here, for a `native` one).
async fn bump(conn: &mut PgConnection, subject_id: Uuid, rating: i16, sign: i32, native: bool) -> sqlx::Result<()> {
    sqlx::query(
        "UPDATE review_subjects SET review_count = review_count + $2, rating_sum = rating_sum + $2 * $3,
            rating_counts[$3] = rating_counts[$3] + $2,
            native_count = native_count + CASE WHEN $4 THEN $2 ELSE 0 END, native_sum = native_sum + CASE WHEN $4 THEN $2 * $3 ELSE 0 END,
            updated_at = now() WHERE id = $1",
    )
    .bind(subject_id)
    .bind(sign)
    .bind(i32::from(rating))
    .bind(native)
    .execute(conn)
    .await?;
    Ok(())
}

/// Something shown about a published review changed (a reply, a photo, featuring): the product's
/// widgets must not answer from a stale cache.
pub async fn touch(conn: &mut PgConnection, review_id: Uuid) -> sqlx::Result<()> {
    sqlx::query("UPDATE review_subjects SET updated_at = now() WHERE id = (SELECT subject_id FROM reviews WHERE id = $1)").bind(review_id).execute(conn).await?;
    Ok(())
}

/// Move reviews to `status` (one of [`STATUSES`]) and keep the summaries right, in the caller's
/// transaction: the rows are locked, and only those whose status changes are written. Returns
/// the ids that changed. A review waiting for its email confirmation can't be published.
pub async fn set_status(conn: &mut PgConnection, account_id: Uuid, ids: &[Uuid], status: &str, reason: Option<&str>) -> AppResult<Vec<Uuid>> {
    let rows: Vec<(Uuid, Uuid, i16, String, bool, String)> = sqlx::query_as(
        "SELECT id, subject_id, rating, status, confirmed_at IS NOT NULL, source FROM reviews WHERE account_id = $1 AND id = ANY($2) ORDER BY id FOR UPDATE",
    )
    .bind(account_id)
    .bind(ids)
    .fetch_all(&mut *conn)
    .await?;
    let mut changed = Vec::new();
    for (id, subject, rating, was, confirmed, source) in rows {
        if was == status || was == "removed" || (status == "published" && !confirmed) {
            continue;
        }
        sqlx::query(
            "UPDATE reviews SET status = $2, reject_reason = $3, moderated_at = now(), updated_at = now(),
                published_at = CASE WHEN $2 = 'published' THEN COALESCE(published_at, now()) ELSE published_at END WHERE id = $1",
        )
        .bind(id)
        .bind(status)
        .bind(if status == "rejected" { reason } else { None })
        .execute(&mut *conn)
        .await?;
        let native = source != "import";
        if was == "published" {
            bump(conn, subject, rating, -1, native).await?;
        }
        if status == "published" {
            bump(conn, subject, rating, 1, native).await?;
        }
        changed.push(id);
    }
    Ok(changed)
}

/// A review written by its author (through their link) moves from what it was to `new_status` with
/// a new rating, keeping the summary right. Used for edits within the edit window.
pub async fn replace_rating(conn: &mut PgConnection, old: (&str, i16, Uuid), new_status: &str, new_rating: i16) -> sqlx::Result<()> {
    let (was, old_rating, subject) = old;
    if was == "published" {
        bump(conn, subject, old_rating, -1, true).await?;
    }
    if new_status == "published" {
        bump(conn, subject, new_rating, 1, true).await?;
    }
    Ok(())
}

/// Delete reviews (and their photos) for good, keeping the summaries right. Their links stay used
/// up (`submitted_at` is kept): deleting a review never lets its link write another.
pub async fn delete_reviews(conn: &mut PgConnection, account_id: Uuid, ids: &[Uuid]) -> sqlx::Result<u64> {
    let rows: Vec<(Uuid, Uuid, i16, String, String)> = sqlx::query_as("SELECT id, subject_id, rating, status, source FROM reviews WHERE account_id = $1 AND id = ANY($2) ORDER BY id FOR UPDATE")
        .bind(account_id)
        .bind(ids)
        .fetch_all(&mut *conn)
        .await?;
    for (_, subject, rating, status, source) in &rows {
        if status == "published" {
            bump(conn, *subject, *rating, -1, source != "import").await?;
        }
    }
    let gone: Vec<Uuid> = rows.iter().map(|r| r.0).collect();
    sqlx::query("UPDATE review_requests SET review_id = NULL, submitted_at = COALESCE(submitted_at, now()) WHERE review_id = ANY($1)").bind(&gone).execute(&mut *conn).await?;
    Ok(sqlx::query("DELETE FROM reviews WHERE id = ANY($1)").bind(&gone).execute(&mut *conn).await?.rows_affected())
}

/// What a deleted profile held (for the callers' webhooks and audit entries).
pub type DeletedProfile = (Option<String>, Option<String>, Option<String>);

/// Delete a person and everything they wrote here, for the privacy tools and profile deletion:
/// the profile row is locked first (which also holds back any new review or request for them),
/// then their reviews and photos go with the product summaries corrected, then the profile, all in
/// one transaction. None when there's no such profile in this account.
pub async fn delete_person(db: &PgPool, account_id: Uuid, profile_id: Uuid) -> AppResult<Option<DeletedProfile>> {
    let mut tx = db.begin().await?;
    let found: Option<Uuid> = sqlx::query_scalar("SELECT id FROM profiles WHERE id = $1 AND account_id = $2 FOR UPDATE").bind(profile_id).bind(account_id).fetch_optional(&mut *tx).await?;
    if found.is_none() {
        return Ok(None);
    }
    super::hooks::point("delete_person", account_id).await.map_err(AppError::Internal)?;
    // Then the person's requests: the order a review being saved takes them in (profile, request).
    sqlx::query("SELECT id FROM review_requests WHERE profile_id = $1 ORDER BY id FOR UPDATE").bind(profile_id).execute(&mut *tx).await?;
    let ids: Vec<Uuid> = sqlx::query_scalar("SELECT id FROM reviews WHERE account_id = $1 AND profile_id = $2").bind(account_id).bind(profile_id).fetch_all(&mut *tx).await?;
    delete_reviews(&mut tx, account_id, &ids).await?;
    sqlx::query("DELETE FROM review_photos WHERE request_id IN (SELECT id FROM review_requests WHERE profile_id = $1)").bind(profile_id).execute(&mut *tx).await?;
    let gone: Option<DeletedProfile> =
        sqlx::query_as("DELETE FROM profiles WHERE id = $1 AND account_id = $2 RETURNING email, phone_number, external_id").bind(profile_id).bind(account_id).fetch_optional(&mut *tx).await?;
    tx.commit().await?;
    Ok(gone)
}

/// Recompute the published summaries of an account's subjects from their reviews. Each subject
/// row is locked first, so a publish running at the same time is either counted here or applies
/// its change after.
pub async fn rebuild_summaries(db: &PgPool, account_id: Uuid) -> AppResult<u64> {
    let ids: Vec<Uuid> = sqlx::query_scalar("SELECT id FROM review_subjects WHERE account_id = $1 ORDER BY id").bind(account_id).fetch_all(db).await?;
    let mut n = 0;
    for chunk in ids.chunks(200) {
        let mut tx = db.begin().await?;
        sqlx::query("SELECT id FROM review_subjects WHERE id = ANY($1) ORDER BY id FOR UPDATE").bind(chunk).execute(&mut *tx).await?;
        n += sqlx::query(
            "UPDATE review_subjects s SET review_count = COALESCE(x.n, 0), rating_sum = COALESCE(x.total, 0),
                rating_counts = ARRAY[COALESCE(x.c1, 0), COALESCE(x.c2, 0), COALESCE(x.c3, 0), COALESCE(x.c4, 0), COALESCE(x.c5, 0)],
                native_count = COALESCE(x.nn, 0), native_sum = COALESCE(x.ns, 0), updated_at = now()
             FROM (SELECT sid AS subject_id, n, total, c1, c2, c3, c4, c5, nn, ns FROM unnest($1::uuid[]) sid LEFT JOIN LATERAL (
                     SELECT count(*)::int AS n, sum(rating)::bigint AS total,
                        count(*) FILTER (WHERE rating = 1)::int AS c1, count(*) FILTER (WHERE rating = 2)::int AS c2, count(*) FILTER (WHERE rating = 3)::int AS c3,
                        count(*) FILTER (WHERE rating = 4)::int AS c4, count(*) FILTER (WHERE rating = 5)::int AS c5,
                        count(*) FILTER (WHERE source <> 'import')::int AS nn, COALESCE(sum(rating) FILTER (WHERE source <> 'import'), 0)::bigint AS ns
                     FROM reviews r WHERE r.subject_id = sid AND r.status = 'published') agg ON true) x
             WHERE s.id = x.subject_id AND (s.review_count, s.rating_sum, s.rating_counts, s.native_count, s.native_sum) IS DISTINCT FROM
                (COALESCE(x.n, 0), COALESCE(x.total, 0)::bigint, ARRAY[COALESCE(x.c1, 0), COALESCE(x.c2, 0), COALESCE(x.c3, 0), COALESCE(x.c4, 0), COALESCE(x.c5, 0)],
                 COALESCE(x.nn, 0), COALESCE(x.ns, 0)::bigint)",
        )
        .bind(chunk)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        tx.commit().await?;
    }
    Ok(n)
}

// ---------- Listing for the dashboard ----------

#[derive(Debug, Default, Deserialize)]
pub struct ListQuery {
    pub status: Option<String>,
    pub rating: Option<i16>,
    pub product_id: Option<String>,
    pub verified: Option<bool>,
    pub has_photos: Option<bool>,
    pub source: Option<String>,
    /// `true`: only public reviews still waiting for their email confirmation.
    pub unconfirmed: Option<bool>,
    pub since: Option<String>,
    pub until: Option<String>,
    pub q: Option<String>,
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

fn day(s: &Option<String>) -> AppResult<Option<chrono::NaiveDate>> {
    s.as_deref().map(str::trim).filter(|s| !s.is_empty()).map(|s| chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d").map_err(|_| AppError::bad("dates are YYYY-MM-DD"))).transpose()
}

pub async fn list(db: &PgPool, account_id: Uuid, tz: &str, q: &ListQuery) -> AppResult<(Vec<(Review, Subject, i64)>, i64)> {
    if let Some(s) = &q.status {
        if !STATUSES.contains(&s.as_str()) {
            return Err(AppError::bad("status must be pending, published, rejected or removed"));
        }
    }
    let mut qb = sqlx::QueryBuilder::<sqlx::Postgres>::new(format!(
        "SELECT {REVIEW_COLS}, (SELECT count(*) FROM review_photos ph WHERE ph.review_id = r.id) AS photos, count(*) OVER () AS total
         FROM reviews r JOIN review_subjects s ON s.id = r.subject_id WHERE r.account_id = "
    ));
    qb.push_bind(account_id);
    if let Some(s) = &q.status {
        qb.push(" AND r.status = ").push_bind(s.clone());
    }
    if let Some(r) = q.rating {
        qb.push(" AND r.rating = ").push_bind(r);
    }
    if let Some(p) = q.product_id.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        qb.push(" AND s.external_id = ").push_bind(p.to_string());
    }
    if let Some(v) = q.verified {
        qb.push(" AND r.verified = ").push_bind(v);
    }
    if let Some(src) = q.source.as_deref().filter(|s| !s.is_empty()) {
        qb.push(" AND r.source = ").push_bind(src.to_string());
    }
    match q.has_photos {
        Some(true) => {
            qb.push(" AND EXISTS (SELECT 1 FROM review_photos ph WHERE ph.review_id = r.id)");
        }
        Some(false) => {
            qb.push(" AND NOT EXISTS (SELECT 1 FROM review_photos ph WHERE ph.review_id = r.id)");
        }
        None => {}
    }
    match q.unconfirmed {
        Some(true) => {
            qb.push(" AND r.confirmed_at IS NULL");
        }
        Some(false) => {
            qb.push(" AND r.confirmed_at IS NOT NULL");
        }
        None => {}
    }
    // Day boundaries in the account's time zone.
    if let Some(d) = day(&q.since)? {
        qb.push(" AND r.reviewed_at >= (").push_bind(d).push("::date::timestamp AT TIME ZONE ").push_bind(tz.to_string()).push(")");
    }
    if let Some(d) = day(&q.until)? {
        qb.push(" AND r.reviewed_at < ((").push_bind(d).push("::date + 1)::timestamp AT TIME ZONE ").push_bind(tz.to_string()).push(")");
    }
    if let Some(text) = q.q.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        let like = format!("%{}%", text.chars().take(100).collect::<String>().replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_"));
        qb.push(" AND (r.title ILIKE ").push_bind(like.clone()).push(" OR r.body ILIKE ").push_bind(like.clone()).push(" OR r.display_name ILIKE ").push_bind(like).push(")");
    }
    qb.push(" ORDER BY r.created_at DESC, r.id DESC LIMIT ").push_bind(q.limit.unwrap_or(50).clamp(1, 200)).push(" OFFSET ").push_bind(q.offset.unwrap_or(0).clamp(0, 1_000_000));
    use sqlx::Row;
    let rows = qb.build().fetch_all(db).await?;
    let mut total = 0;
    let mut reviews = Vec::new();
    for row in &rows {
        total = row.try_get::<i64, _>("total")?;
        reviews.push((<Review as sqlx::FromRow<sqlx::postgres::PgRow>>::from_row(row)?, row.try_get::<i64, _>("photos")?));
    }
    let ids: Vec<Uuid> = reviews.iter().map(|(r, _)| r.subject_id).collect();
    let sq = format!("SELECT {SUBJECT_COLS} FROM review_subjects s WHERE s.id = ANY($1)");
    let subjects: HashMap<Uuid, Subject> = sqlx::query_as::<_, Subject>(&sq).bind(&ids).fetch_all(db).await?.into_iter().map(|s| (s.id, s)).collect();
    let mut out = Vec::new();
    for (r, photos) in reviews {
        if let Some(s) = subjects.get(&r.subject_id) {
            out.push((r, s.clone(), photos));
        }
    }
    Ok((out, total))
}

/// Photos of reviews, by review id: (photo id, width, height).
pub async fn photos_of(db: &PgPool, review_ids: &[Uuid]) -> AppResult<HashMap<Uuid, Vec<(Uuid, i32, i32)>>> {
    let rows: Vec<(Uuid, Uuid, i32, i32)> =
        sqlx::query_as("SELECT review_id, id, width, height FROM review_photos WHERE review_id = ANY($1) ORDER BY position, created_at").bind(review_ids).fetch_all(db).await?;
    let mut m: HashMap<Uuid, Vec<(Uuid, i32, i32)>> = HashMap::new();
    for (r, id, w, h) in rows {
        m.entry(r).or_default().push((id, w, h));
    }
    Ok(m)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flags_catch_contact_details_links_and_words() {
        assert!(flags("Lovely mug, use it daily", &[]).is_empty());
        assert_eq!(flags("email me at jo@example.com", &[]), vec!["email"]);
        assert_eq!(flags("call 503 555 0100 now", &[]), vec!["phone"]);
        assert_eq!(flags("see https://spam.test for deals", &[]), vec!["link"]);
        assert_eq!(flags("This is SHIT", &[]), vec!["profanity"]);
        assert_eq!(flags("Competitor brand is better", &["competitor".into()]), vec!["word:competitor"]);
        assert!(flags("a scunthorpe resident", &[]).is_empty(), "matches whole words only");
    }

    #[test]
    fn default_names_and_plain_text() {
        assert_eq!(default_name(true, false), "A verified buyer");
        assert_eq!(default_name(true, true), "A guest");
        assert_eq!(default_name(false, false), "A customer");
        assert_eq!(plain("  hi\u{0}\n\n\n\nthere  ", 100), "hi\n\nthere");
        assert_eq!(plain("abcdef", 3), "abc");
    }

    #[test]
    fn auto_publish_needs_verified_stars_and_no_flags() {
        let s = Settings { auto_publish: true, auto_publish_min_rating: 4, flag_words: vec![], questions: json!([]), public_reviews: false, request_days: 60, thank_you_coupon: None, subscribe_form_id: None, widget: json!({}), purchase_metrics: vec![] };
        assert!(auto_publishes(&s, true, 4, &[]));
        assert!(!auto_publishes(&s, false, 5, &[]));
        assert!(!auto_publishes(&s, true, 3, &[]));
        assert!(!auto_publishes(&s, true, 5, &["link".into()]));
    }

    #[test]
    fn questions_are_checked() {
        assert!(check_questions(&json!([{"label": "Fit", "kind": "choice", "options": ["Small", "True to size", "Large"]}])).is_ok());
        assert!(check_questions(&json!([{"label": "a"}, {"label": "b"}, {"label": "c"}, {"label": "d"}])).is_err());
        assert!(check_questions(&json!([{"label": "Fit", "kind": "choice", "options": ["only one"]}])).is_err());
        let q = check_questions(&json!([{"id": "fit", "label": "Fit", "kind": "choice", "options": ["S", "M"]}, {"id": "use", "label": "Used for"}])).unwrap();
        let a = check_answers(&q, &json!({"fit": "M", "use": "hiking", "other": "x"})).unwrap();
        assert_eq!(a.as_array().unwrap().len(), 2);
        assert!(check_answers(&q, &json!({"fit": "XL"})).is_err());
    }
}
