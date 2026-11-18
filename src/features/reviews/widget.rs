//! Public, read-only review endpoints for the website widgets (by site key), the widget script
//! itself, and the product's structured data.
//!
//! Only published reviews and their display names, dates, photos and replies leave here: never an
//! email address, a profile id or anything unpublished. A product's name, image and page address
//! appear only when a trusted source supplied them. Reads are throttled per visitor address, never
//! count rows (summaries come from the stored summary row; lists are keyset pages with a `next`
//! cursor), and answer a matching `ETag` from the summary's version before any list query runs.

use axum::extract::{Query, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use super::photos;
use super::store::{self, Review, Subject, SUBJECT_COLS};
use crate::error::{AppError, AppResult};
use crate::models::Account;
use crate::state::AppState;

/// Public review reads one visitor address may make a minute.
const READS_PER_MINUTE: i32 = 300;
/// The most reviews structured data marks up.
const LD_REVIEWS: usize = 10;

pub fn cors(mut r: Response) -> Response {
    let h = r.headers_mut();
    h.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, HeaderValue::from_static("*"));
    h.insert(header::ACCESS_CONTROL_ALLOW_METHODS, HeaderValue::from_static("GET, POST, OPTIONS"));
    h.insert(header::ACCESS_CONTROL_ALLOW_HEADERS, HeaderValue::from_static("content-type"));
    r
}

pub async fn preflight() -> Response {
    cors(StatusCode::NO_CONTENT.into_response())
}

async fn account(state: &AppState, k: &str) -> AppResult<Account> {
    if k.is_empty() || k.len() > 100 {
        return Err(AppError::Unauthorized);
    }
    sqlx::query_as("SELECT * FROM accounts WHERE public_key = $1").bind(k).fetch_optional(&state.db).await?.ok_or(AppError::Unauthorized)
}

async fn throttle(state: &AppState, headers: &HeaderMap) -> AppResult<()> {
    let ip = crate::public::client_ip(headers);
    crate::features::team::ratelimit::hit_ip(state, "review-read", ip.as_deref(), READS_PER_MINUTE, 60).await
}

/// An ETag from what the answer depends on, known before the answer is built.
fn tag_of(parts: &[&str]) -> String {
    let mut h = Sha256::new();
    for p in parts {
        h.update(p.as_bytes());
        h.update([0u8]);
    }
    format!("\"{}\"", &hex::encode(h.finalize())[..32])
}

fn matches_tag(headers: &HeaderMap, tag: &str) -> bool {
    headers.get(header::IF_NONE_MATCH).and_then(|v| v.to_str().ok()).is_some_and(|v| v.split(',').any(|t| t.trim().trim_start_matches("W/") == tag))
}

fn with_cache(mut r: Response, tag: &str) -> Response {
    let h = r.headers_mut();
    if let Ok(v) = HeaderValue::from_str(tag) {
        h.insert(header::ETAG, v);
    }
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("public, max-age=60"));
    cors(r)
}

fn not_modified(tag: &str) -> Response {
    with_cache(StatusCode::NOT_MODIFIED.into_response(), tag)
}

fn json_answer(body: &Value, tag: &str, content_type: &'static str) -> Response {
    with_cache(([(header::CONTENT_TYPE, content_type)], body.to_string()).into_response(), tag)
}

fn fail(e: AppError) -> Response {
    cors(e.into_response())
}

fn date_in(account: &Account, at: DateTime<Utc>) -> String {
    at.with_timezone(&crate::sending::account_tz(account)).format("%Y-%m-%d").to_string()
}

/// When the account's review settings (the widget's look) last changed.
async fn settings_version(state: &AppState, account_id: Uuid) -> AppResult<String> {
    let at: Option<DateTime<Utc>> = sqlx::query_scalar("SELECT updated_at FROM review_settings WHERE account_id = $1").bind(account_id).fetch_optional(&state.db).await?;
    Ok(at.map(|t| t.timestamp_micros().to_string()).unwrap_or_default())
}

/// A published review as the public sees it. Imported reviews never carry the verified badge.
pub fn public_review(state: &AppState, account: &Account, r: &Review, photo_ids: &[(Uuid, i32, i32)], subject: Option<&Subject>) -> Value {
    let base = crate::domains::link_base(state, account);
    let mut v = json!({
        "id": r.id, "rating": r.rating, "title": r.title, "body": r.body, "name": r.display_name, "date": date_in(account, r.reviewed_at),
        "verified": r.verified && r.source != "import", "incentivized": r.incentivized, "recommend": r.recommend, "featured": r.featured,
        "answers": r.answers.as_array().into_iter().flatten().map(|a| json!({"label": a["label"], "answer": a["answer"]})).collect::<Vec<_>>(),
        "photos": photo_ids.iter().map(|(id, w, h)| json!({"url": photos::public_url(&base, *id), "width": w, "height": h})).collect::<Vec<_>>(),
        "reply": r.reply.as_ref().map(|t| json!({"text": t, "date": r.replied_at.map(|d| date_in(account, d))})),
    });
    if let Some(s) = subject {
        v["product"] = s.public_card();
    }
    v
}

/// The order of a list, as a key `(k, reviewed_at, id)` read in descending order, so every sort
/// pages by keyset.
fn sort_key(sort: Option<&str>) -> &'static str {
    match sort {
        Some("highest") => "r.rating::int",
        Some("lowest") => "-r.rating::int",
        Some("recent") => "0",
        _ => "CASE WHEN r.featured THEN 1 ELSE 0 END",
    }
}

/// A position in a list: the key of the last review shown.
fn cursor_of(k: i32, at: DateTime<Utc>, id: Uuid) -> String {
    format!("{k}_{}_{}", at.timestamp_micros(), id.simple())
}

fn parse_cursor(c: &str) -> AppResult<(i32, DateTime<Utc>, Uuid)> {
    let bad = || AppError::bad("after isn't a cursor from this list");
    let mut it = c.splitn(3, '_');
    let k: i32 = it.next().and_then(|x| x.parse().ok()).ok_or_else(bad)?;
    let at = it.next().and_then(|x| x.parse::<i64>().ok()).and_then(DateTime::from_timestamp_micros).ok_or_else(bad)?;
    let id = it.next().and_then(|x| Uuid::parse_str(x).ok()).ok_or_else(bad)?;
    Ok((k, at, id))
}

struct Page {
    rows: Vec<Review>,
    next: Option<String>,
}

/// One keyset page of published reviews (for one subject, or the whole account).
#[allow(clippy::too_many_arguments)]
async fn published(state: &AppState, account: &Account, subject: Option<Uuid>, rating: Option<i16>, with_photos: bool, featured: bool, sort: Option<&str>, after: Option<&str>, limit: i64) -> AppResult<Page> {
    let key = sort_key(sort);
    let after = after.filter(|a| !a.is_empty()).map(parse_cursor).transpose()?;
    let q = format!(
        "SELECT {}, {key} AS sort_k FROM reviews r WHERE r.account_id = $1 AND r.status = 'published'
           AND ($2::uuid IS NULL OR r.subject_id = $2) AND ($3::smallint IS NULL OR r.rating = $3)
           AND (NOT $4 OR EXISTS (SELECT 1 FROM review_photos ph WHERE ph.review_id = r.id)) AND (NOT $5 OR r.featured)
           AND ($6::int IS NULL OR ({key}, r.reviewed_at, r.id) < ($6, $7, $8))
         ORDER BY {key} DESC, r.reviewed_at DESC, r.id DESC LIMIT $9",
        store::REVIEW_COLS
    );
    use sqlx::Row;
    let rows = sqlx::query(&q)
        .bind(account.id)
        .bind(subject)
        .bind(rating)
        .bind(with_photos)
        .bind(featured)
        .bind(after.map(|a| a.0))
        .bind(after.map(|a| a.1))
        .bind(after.map(|a| a.2))
        .bind(limit + 1)
        .fetch_all(&state.db)
        .await?;
    let mut out = Vec::new();
    let mut keys = Vec::new();
    for row in &rows {
        keys.push(row.try_get::<i32, _>("sort_k")?);
        out.push(<Review as sqlx::FromRow<sqlx::postgres::PgRow>>::from_row(row)?);
    }
    let more = out.len() as i64 > limit;
    out.truncate(limit as usize);
    let next = more.then(|| out.last().map(|r| cursor_of(keys[out.len() - 1], r.reviewed_at, r.id))).flatten();
    Ok(Page { rows: out, next })
}

fn per_page(s: &store::Settings) -> i64 {
    s.widget["per_page"].as_i64().unwrap_or(5).clamp(3, 20)
}

fn widget_settings(account: &Account, s: &store::Settings) -> Value {
    json!({
        "heading": s.widget["heading"].as_str().filter(|h| !h.is_empty()).unwrap_or("Customer reviews"),
        "star_color": s.widget["star_color"].as_str().filter(|c| !c.is_empty()).unwrap_or("#f59e0b"),
        "text_color": s.widget["text_color"].as_str().filter(|c| !c.is_empty()).unwrap_or("#111827"),
        "accent_color": s.widget["accent_color"].as_str().filter(|c| !c.is_empty()).map(String::from).unwrap_or_else(|| crate::render::safe_color(&account.brand_color, "#1f6f5c")),
    })
}

/// Google's review snippet markup for a product page, built from the reviews the widget shows
/// there: a `Product` with the `AggregateRating` of the reviews collected here and those of the
/// shown reviews that were collected here (never imported ones: Google doesn't allow marking up
/// reviews gathered on other sites). Products only, and only with a name a trusted source gave:
/// Google shows no stars for a business's reviews of itself (LocalBusiness, Organization), so
/// listings and services get none.
fn structured_data(s: &Subject, shown: &[Review], account: &Account) -> Option<Value> {
    let name = s.trusted_name()?;
    if s.kind != "product" || s.native_count == 0 {
        return None;
    }
    let reviews: Vec<Value> = shown
        .iter()
        .filter(|r| r.source != "import")
        .take(LD_REVIEWS)
        .map(|r| {
            json!({"@type": "Review", "author": {"@type": "Person", "name": r.display_name.chars().take(99).collect::<String>()},
                "datePublished": date_in(account, r.reviewed_at), "reviewRating": {"@type": "Rating", "ratingValue": r.rating, "bestRating": 5, "worstRating": 1},
                "name": (!r.title.is_empty()).then_some(&r.title), "reviewBody": (!r.body.is_empty()).then_some(&r.body)})
        })
        .collect();
    let mut v = json!({"@context": "https://schema.org", "@type": "Product", "name": name, "sku": s.external_id,
        "aggregateRating": {"@type": "AggregateRating", "ratingValue": s.native_average(), "reviewCount": s.native_count, "bestRating": 5, "worstRating": 1},
        "review": reviews});
    if let Some(i) = s.trusted_image() {
        v["image"] = json!(i);
    }
    if let Some(u) = s.trusted_url() {
        v["url"] = json!(u);
    }
    Some(v)
}

#[derive(Deserialize)]
pub struct ListQuery {
    k: String,
    #[serde(default)]
    product_id: Option<String>,
    #[serde(default)]
    sort: Option<String>,
    #[serde(default)]
    rating: Option<i16>,
    #[serde(default)]
    photos: Option<bool>,
    /// The `next` cursor of the page before.
    #[serde(default)]
    after: Option<String>,
}

/// One product's published reviews with its summary. The first page with the default order and no
/// filters carries the structured data for exactly the reviews it shows.
pub async fn product_reviews(State(state): State<AppState>, headers: HeaderMap, Query(q): Query<ListQuery>) -> Response {
    let res: AppResult<Response> = async {
        throttle(&state, &headers).await?;
        let account = account(&state, &q.k).await?;
        let key = q.product_id.as_deref().and_then(store::valid_key).ok_or_else(|| AppError::bad("product_id is missing"))?;
        let rating = q.rating.filter(|r| (1..=5).contains(r));
        let subject = store::find_subject(&state.db, account.id, &key).await?;
        let version = subject.as_ref().map(|s| s.updated_at.timestamp_micros().to_string()).unwrap_or_default();
        let tag = tag_of(&[&account.id.to_string(), &key, &version, &settings_version(&state, account.id).await?, q.sort.as_deref().unwrap_or(""), &rating.map(|r| r.to_string()).unwrap_or_default(),
            if q.photos == Some(true) { "p" } else { "" }, q.after.as_deref().unwrap_or("")]);
        if matches_tag(&headers, &tag) {
            return Ok(not_modified(&tag));
        }
        let s = store::settings(&state.db, account.id).await?;
        let per = per_page(&s);
        let questions: Vec<Value> = s.questions.as_array().cloned().unwrap_or_default();
        let mut v = json!({"widget": widget_settings(&account, &s), "can_write": s.public_reviews, "questions": questions, "per_page": per, "next": null, "structured_data": null});
        let Some(subject) = subject else {
            v["product"] = json!({"product_id": key});
            v["summary"] = json!({"average": null, "count": 0, "histogram": {"5": 0, "4": 0, "3": 0, "2": 0, "1": 0}});
            v["reviews"] = json!([]);
            return Ok(json_answer(&v, &tag, "application/json"));
        };
        let page = published(&state, &account, Some(subject.id), rating, q.photos == Some(true), false, q.sort.as_deref(), q.after.as_deref(), per).await?;
        let photos = store::photos_of(&state.db, &page.rows.iter().map(|r| r.id).collect::<Vec<_>>()).await?;
        let first_default = q.after.is_none() && q.sort.is_none() && rating.is_none() && q.photos != Some(true);
        if first_default {
            v["structured_data"] = json!(structured_data(&subject, &page.rows, &account));
        }
        v["product"] = subject.public_card();
        v["summary"] = subject.summary();
        v["reviews"] = json!(page.rows.iter().map(|r| public_review(&state, &account, r, photos.get(&r.id).map(|p| p.as_slice()).unwrap_or(&[]), None)).collect::<Vec<_>>());
        v["next"] = json!(page.next);
        Ok(json_answer(&v, &tag, "application/json"))
    }
    .await;
    res.unwrap_or_else(fail)
}

#[derive(Deserialize)]
pub struct SummaryQuery {
    k: String,
    ids: String,
}

/// Star badges for product cards: average and count for up to 50 products at once.
pub async fn summaries(State(state): State<AppState>, headers: HeaderMap, Query(q): Query<SummaryQuery>) -> Response {
    let res: AppResult<Response> = async {
        throttle(&state, &headers).await?;
        let account = account(&state, &q.k).await?;
        let ids: Vec<String> = q.ids.split(',').filter_map(store::valid_key).take(50).collect();
        let sql = format!("SELECT {SUBJECT_COLS} FROM review_subjects s WHERE s.account_id = $1 AND s.external_id = ANY($2)");
        let rows: Vec<Subject> = sqlx::query_as(&sql).bind(account.id).bind(&ids).fetch_all(&state.db).await?;
        let mut data = serde_json::Map::new();
        for s in rows {
            data.insert(s.external_id.clone(), json!({"average": s.average(), "count": s.review_count}));
        }
        let v = json!({"data": data, "widget": widget_settings(&account, &store::settings(&state.db, account.id).await?)});
        let tag = tag_of(&[&v.to_string()]);
        if matches_tag(&headers, &tag) {
            return Ok(not_modified(&tag));
        }
        Ok(json_answer(&v, &tag, "application/json"))
    }
    .await;
    res.unwrap_or_else(fail)
}

#[derive(Deserialize)]
pub struct AllQuery {
    k: String,
    #[serde(default)]
    after: Option<String>,
    #[serde(default)]
    sort: Option<String>,
    #[serde(default)]
    featured: Option<bool>,
    #[serde(default)]
    limit: Option<i64>,
}

/// Every product's published reviews (the all-reviews page), or the featured ones (the carousel;
/// the newest when none are featured).
pub async fn all_reviews(State(state): State<AppState>, headers: HeaderMap, Query(q): Query<AllQuery>) -> Response {
    let res: AppResult<Response> = async {
        throttle(&state, &headers).await?;
        let account = account(&state, &q.k).await?;
        let (latest, count, sum): (Option<DateTime<Utc>>, i64, i64) = sqlx::query_as(
            "SELECT max(updated_at), COALESCE(sum(review_count), 0)::bigint, COALESCE(sum(rating_sum), 0)::bigint FROM review_subjects WHERE account_id = $1",
        )
        .bind(account.id)
        .fetch_one(&state.db)
        .await?;
        let featured = q.featured == Some(true);
        let tag = tag_of(&[&account.id.to_string(), &latest.map(|t| t.timestamp_micros().to_string()).unwrap_or_default(), &settings_version(&state, account.id).await?,
            q.sort.as_deref().unwrap_or(""), if featured { "f" } else { "" }, q.after.as_deref().unwrap_or(""), &q.limit.unwrap_or(0).to_string()]);
        if matches_tag(&headers, &tag) {
            return Ok(not_modified(&tag));
        }
        let s = store::settings(&state.db, account.id).await?;
        let per = q.limit.unwrap_or(per_page(&s)).clamp(1, 20);
        let mut page = if featured { published(&state, &account, None, None, false, true, Some("recent"), None, per).await? } else { Page { rows: Vec::new(), next: None } };
        if !featured || page.rows.is_empty() {
            page = published(&state, &account, None, None, false, false, if featured { Some("recent") } else { q.sort.as_deref() }, if featured { None } else { q.after.as_deref() }, per).await?;
        }
        if featured {
            page.next = None;
        }
        let photos = store::photos_of(&state.db, &page.rows.iter().map(|r| r.id).collect::<Vec<_>>()).await?;
        let sids: Vec<Uuid> = page.rows.iter().map(|r| r.subject_id).collect();
        let sql = format!("SELECT {SUBJECT_COLS} FROM review_subjects s WHERE s.id = ANY($1)");
        let subjects: Vec<Subject> = sqlx::query_as(&sql).bind(&sids).fetch_all(&state.db).await?;
        let list: Vec<Value> =
            page.rows.iter().map(|r| public_review(&state, &account, r, photos.get(&r.id).map(|p| p.as_slice()).unwrap_or(&[]), subjects.iter().find(|x| x.id == r.subject_id))).collect();
        let v = json!({"widget": widget_settings(&account, &s), "reviews": list, "per_page": per, "next": page.next,
            "summary": {"count": count, "average": (count > 0).then(|| (sum as f64 / count as f64 * 10.0).round() / 10.0)}});
        Ok(json_answer(&v, &tag, "application/json"))
    }
    .await;
    res.unwrap_or_else(fail)
}

/// The structured data for a product's page, for sites that render it into their own template:
/// the same markup the product widget adds for its first page.
pub async fn json_ld(state: &AppState, account: &Account, key: &str) -> AppResult<Option<Value>> {
    let Some(s) = store::find_subject(&state.db, account.id, key).await? else { return Ok(None) };
    let per = per_page(&store::settings(&state.db, account.id).await?);
    let page = published(state, account, Some(s.id), None, false, false, None, None, per).await?;
    Ok(structured_data(&s, &page.rows, account))
}

#[derive(Deserialize)]
pub struct LdQuery {
    k: String,
    product_id: String,
}

pub async fn json_ld_h(State(state): State<AppState>, headers: HeaderMap, Query(q): Query<LdQuery>) -> Response {
    let res: AppResult<Response> = async {
        throttle(&state, &headers).await?;
        let account = account(&state, &q.k).await?;
        let key = store::valid_key(&q.product_id).ok_or_else(|| AppError::bad("product_id is missing"))?;
        let v = json_ld(&state, &account, &key).await?.ok_or_else(|| AppError::not_found("reviews for this product"))?;
        let tag = tag_of(&[&v.to_string()]);
        Ok(json_answer(&v, &tag, "application/ld+json"))
    }
    .await;
    res.unwrap_or_else(fail)
}

const WIDGET_JS: &str = include_str!("widget.js");

#[derive(Deserialize)]
pub struct KeyQuery {
    k: String,
}

/// The widget script. The website script loads it when a page has a review widget, so sites never
/// add a second script tag.
pub async fn widget_js(State(state): State<AppState>, headers: HeaderMap, Query(q): Query<KeyQuery>) -> Response {
    let base = state.cfg.public_base_for(&crate::site::request_host(&headers));
    let js = WIDGET_JS.replacen("__BASE__", &serde_json::to_string(base).unwrap_or_default(), 1).replacen("__KEY__", &serde_json::to_string(&q.k).unwrap_or_default(), 1);
    ([(header::CONTENT_TYPE, "application/javascript; charset=utf-8"), (header::CACHE_CONTROL, "public, max-age=300")], js).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursors_round_trip_and_reject_junk() {
        let at = DateTime::from_timestamp_micros(1_700_000_000_123_456).unwrap();
        let id = Uuid::now_v7();
        let c = cursor_of(-4, at, id);
        assert_eq!(parse_cursor(&c).unwrap(), (-4, at, id));
        assert!(parse_cursor("x_1_2").is_err());
        assert!(parse_cursor("1_2").is_err());
    }
}
